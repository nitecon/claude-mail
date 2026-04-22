use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::sync::{Arc, Mutex};

pub type Db = Arc<Mutex<Connection>>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Project {
    pub ident: String,
    /// Name of the channel plugin handling this project ("discord", "slack", …).
    pub channel_name: String,
    /// Opaque, plugin-specific room identifier.
    pub room_id: String,
    /// Opaque ID of the last inbound message seen (backfill cursor).
    pub last_msg_id: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Message {
    pub id: i64,
    pub project_ident: String,
    /// "agent" | "user"
    pub source: String,
    /// Opaque, plugin-specific message identifier.
    pub external_message_id: Option<String>,
    pub content: String,
    pub sent_at: i64,
    /// Timestamp (ms) when the agent confirmed this message, or None if unconfirmed.
    pub confirmed_at: Option<i64>,
    pub parent_message_id: Option<i64>,
    pub agent_id: Option<String>,
    /// "message" | "reply" | "action"
    pub message_type: String,
    /// Short headline supplied by the agent (or auto-derived from the body).
    pub subject: Option<String>,
    /// Origin host the agent claims to be running on (defaults to agent_id).
    pub hostname: Option<String>,
    /// Event time (epoch ms) supplied by the agent — distinct from sent_at,
    /// which is the gateway-receive time.
    pub event_at: Option<i64>,
}

pub fn open(path: &str) -> Result<Db> {
    let conn = Connection::open(path).context("open sqlite database")?;
    conn.execute_batch(
        "PRAGMA journal_mode=WAL;
         PRAGMA foreign_keys=ON;",
    )?;
    apply_schema(&conn)?;
    Ok(Arc::new(Mutex::new(conn)))
}

fn apply_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
            ident         TEXT PRIMARY KEY,
            channel_name  TEXT NOT NULL,
            room_id       TEXT NOT NULL,
            last_msg_id   TEXT,
            created_at    INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS messages (
            id                   INTEGER PRIMARY KEY AUTOINCREMENT,
            project_ident        TEXT NOT NULL REFERENCES projects(ident),
            source               TEXT NOT NULL CHECK(source IN ('agent','user')),
            external_message_id  TEXT,
            content              TEXT NOT NULL,
            sent_at              INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_messages_project
            ON messages(project_ident, id);

        CREATE TABLE IF NOT EXISTS cursors (
            project_ident  TEXT PRIMARY KEY REFERENCES projects(ident),
            last_read_id   INTEGER NOT NULL DEFAULT 0,
            updated_at     INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS skills (
            name        TEXT PRIMARY KEY,
            zip_data    BLOB NOT NULL,
            size        INTEGER NOT NULL,
            checksum    TEXT NOT NULL,
            uploaded_at INTEGER NOT NULL
        );",
    )
    .context("apply schema")?;

    // ── Migration: add per-message confirmation column ────────────────────────
    // Idempotent: ALTER fails silently if the column already exists.
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN confirmed_at INTEGER", []);

    // Migrate old cursor state: mark all previously-read messages as confirmed.
    conn.execute(
        "UPDATE messages SET confirmed_at = sent_at
         WHERE confirmed_at IS NULL
           AND id <= (
               SELECT COALESCE(c.last_read_id, 0)
               FROM cursors c
               WHERE c.project_ident = messages.project_ident
           )",
        [],
    )?;

    // Partial index for fast unconfirmed-message lookups.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_messages_unconfirmed
             ON messages(project_ident, id) WHERE confirmed_at IS NULL;",
    )?;

    // ── Migration: add kind/content columns for command support ───────────────
    let _ = conn.execute(
        "ALTER TABLE skills ADD COLUMN kind TEXT NOT NULL DEFAULT 'skill'",
        [],
    );
    let _ = conn.execute("ALTER TABLE skills ADD COLUMN content TEXT", []);

    // ── Migration: per-agent message buffers ─────────────────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS agents (
            project_ident  TEXT NOT NULL REFERENCES projects(ident),
            agent_id       TEXT NOT NULL,
            registered_at  INTEGER NOT NULL,
            PRIMARY KEY (project_ident, agent_id)
        );

        CREATE TABLE IF NOT EXISTS agent_confirmations (
            agent_id       TEXT NOT NULL,
            project_ident  TEXT NOT NULL,
            message_id     INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
            confirmed_at   INTEGER NOT NULL,
            PRIMARY KEY (agent_id, project_ident, message_id)
        );

        CREATE INDEX IF NOT EXISTS idx_agent_conf_project
            ON agent_confirmations(project_ident, message_id);",
    )?;

    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN parent_message_id INTEGER",
        [],
    );
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN agent_id TEXT", []);
    let _ = conn.execute(
        "ALTER TABLE messages ADD COLUMN message_type TEXT NOT NULL DEFAULT 'message'",
        [],
    );

    // ── Migration: structured-message fields (subject/hostname/event_at) ─────
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN subject TEXT", []);
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN hostname TEXT", []);
    let _ = conn.execute("ALTER TABLE messages ADD COLUMN event_at INTEGER", []);

    // Migrate existing confirmed messages to agent_confirmations for "_default" agent.
    conn.execute(
        "INSERT OR IGNORE INTO agent_confirmations (agent_id, project_ident, message_id, confirmed_at)
         SELECT '_default', project_ident, id, confirmed_at
         FROM messages
         WHERE confirmed_at IS NOT NULL",
        [],
    )?;

    // ── Settings: simple key/value store for UI prefs (theme, etc.) ──────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );",
    )?;

    // ── Tasks: per-project kanban (todo/in_progress/done) ────────────────────
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS tasks (
            id              TEXT PRIMARY KEY,
            project_ident   TEXT NOT NULL REFERENCES projects(ident),
            title           TEXT NOT NULL,
            description     TEXT,
            details         TEXT,
            status          TEXT NOT NULL DEFAULT 'todo'
                            CHECK(status IN ('todo','in_progress','done')),
            rank            INTEGER NOT NULL DEFAULT 0,
            labels          TEXT,
            hostname        TEXT,
            owner_agent_id  TEXT,
            reporter        TEXT NOT NULL,
            created_at      INTEGER NOT NULL,
            updated_at      INTEGER NOT NULL,
            started_at      INTEGER,
            done_at         INTEGER
        );
        CREATE INDEX IF NOT EXISTS idx_tasks_project_status
            ON tasks(project_ident, status);
        CREATE INDEX IF NOT EXISTS idx_tasks_project_rank
            ON tasks(project_ident, status, rank);

        CREATE TABLE IF NOT EXISTS task_comments (
            id           TEXT PRIMARY KEY,
            task_id      TEXT NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
            author       TEXT NOT NULL,
            author_type  TEXT NOT NULL CHECK(author_type IN ('agent','user','system')),
            content      TEXT NOT NULL,
            created_at   INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_task_comments_task
            ON task_comments(task_id, created_at);",
    )?;

    Ok(())
}

// ── Settings ─────────────────────────────────────────────────────────────────

pub fn get_setting(conn: &Connection, key: &str) -> Result<Option<String>> {
    let mut stmt = conn.prepare_cached("SELECT value FROM settings WHERE key = ?1")?;
    let mut rows = stmt.query_map(params![key], |r| r.get::<_, String>(0))?;
    Ok(rows.next().transpose()?)
}

pub fn set_setting(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO settings (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

/// Default theme used when nothing is stored yet.
pub const DEFAULT_THEME: &str = "dark";

pub fn get_theme(conn: &Connection) -> Result<String> {
    Ok(get_setting(conn, "theme")?.unwrap_or_else(|| DEFAULT_THEME.to_string()))
}

pub fn set_theme(conn: &Connection, theme: &str) -> Result<()> {
    set_setting(conn, "theme", theme)
}

// ── Projects ─────────────────────────────────────────────────────────────────

pub fn get_project(conn: &Connection, ident: &str) -> Result<Option<Project>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ident, channel_name, room_id, last_msg_id, created_at
         FROM projects WHERE ident = ?1",
    )?;
    let mut rows = stmt.query_map(params![ident], row_to_project)?;
    Ok(rows.next().transpose()?)
}

/// Find a project by its plugin-specific room_id and channel_name.
pub fn get_project_by_room(
    conn: &Connection,
    channel_name: &str,
    room_id: &str,
) -> Result<Option<Project>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ident, channel_name, room_id, last_msg_id, created_at
         FROM projects WHERE channel_name = ?1 AND room_id = ?2",
    )?;
    let mut rows = stmt.query_map(params![channel_name, room_id], row_to_project)?;
    Ok(rows.next().transpose()?)
}

pub fn insert_project(conn: &Connection, p: &Project) -> Result<()> {
    conn.execute(
        "INSERT INTO projects (ident, channel_name, room_id, last_msg_id, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(ident) DO NOTHING",
        params![
            p.ident,
            p.channel_name,
            p.room_id,
            p.last_msg_id,
            p.created_at
        ],
    )?;
    conn.execute(
        "INSERT INTO cursors (project_ident, last_read_id, updated_at)
         VALUES (?1, 0, ?2)
         ON CONFLICT(project_ident) DO NOTHING",
        params![p.ident, p.created_at],
    )?;
    Ok(())
}

pub fn all_projects(conn: &Connection) -> Result<Vec<Project>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ident, channel_name, room_id, last_msg_id, created_at FROM projects",
    )?;
    let collected = stmt
        .query_map([], row_to_project)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(collected)
}

pub fn update_last_msg_id(conn: &Connection, ident: &str, msg_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE projects SET last_msg_id = ?1 WHERE ident = ?2",
        params![msg_id, ident],
    )?;
    Ok(())
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        ident: row.get(0)?,
        channel_name: row.get(1)?,
        room_id: row.get(2)?,
        last_msg_id: row.get(3)?,
        created_at: row.get(4)?,
    })
}

// ── Messages ─────────────────────────────────────────────────────────────────

pub fn insert_message(conn: &Connection, m: &Message) -> Result<i64> {
    let confirmed_at = if m.source == "agent" {
        Some(now_ms())
    } else {
        None
    };
    conn.execute(
        "INSERT INTO messages (project_ident, source, external_message_id, content, sent_at, confirmed_at, parent_message_id, agent_id, message_type, subject, hostname, event_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            m.project_ident,
            m.source,
            m.external_message_id,
            m.content,
            m.sent_at,
            confirmed_at,
            m.parent_message_id,
            m.agent_id,
            m.message_type,
            m.subject,
            m.hostname,
            m.event_at,
        ],
    )?;
    let msg_id = conn.last_insert_rowid();

    // Auto-confirm for the sending agent so it doesn't appear in their unread queue.
    if m.source == "agent" {
        if let Some(ref aid) = m.agent_id {
            conn.execute(
                "INSERT OR IGNORE INTO agent_confirmations (agent_id, project_ident, message_id, confirmed_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![aid, m.project_ident, msg_id, now_ms()],
            )?;
        }
    }

    Ok(msg_id)
}

/// Lazily register an agent for a project.
pub fn upsert_agent(conn: &Connection, project_ident: &str, agent_id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO agents (project_ident, agent_id, registered_at)
         VALUES (?1, ?2, ?3)",
        params![project_ident, agent_id, now_ms()],
    )?;
    Ok(())
}

/// Get all messages not yet confirmed by a specific agent.
pub fn get_unconfirmed_for_agent(
    conn: &Connection,
    ident: &str,
    agent_id: &str,
) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare_cached(
        "SELECT m.id, m.project_ident, m.source, m.external_message_id,
                m.content, m.sent_at, m.confirmed_at,
                m.parent_message_id, m.agent_id, m.message_type,
                m.subject, m.hostname, m.event_at
         FROM messages m
         WHERE m.project_ident = ?1
           AND NOT EXISTS (
               SELECT 1 FROM agent_confirmations ac
               WHERE ac.agent_id = ?2
                 AND ac.project_ident = ?1
                 AND ac.message_id = m.id
           )
         ORDER BY m.id ASC",
    )?;
    let collected = stmt
        .query_map(params![ident, agent_id], row_to_message)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(collected)
}

/// Confirm a message for a specific agent. Returns true if newly confirmed.
pub fn confirm_message_for_agent(
    conn: &Connection,
    project_ident: &str,
    agent_id: &str,
    msg_id: i64,
) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO agent_confirmations (agent_id, project_ident, message_id, confirmed_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![agent_id, project_ident, msg_id, now_ms()],
    )?;
    Ok(n > 0)
}

/// Fetch a single message by ID within a project.
pub fn get_message_by_id(
    conn: &Connection,
    project_ident: &str,
    msg_id: i64,
) -> Result<Option<Message>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, project_ident, source, external_message_id, content, sent_at, confirmed_at,
                parent_message_id, agent_id, message_type, subject, hostname, event_at
         FROM messages
         WHERE id = ?1 AND project_ident = ?2",
    )?;
    let mut rows = stmt.query_map(params![msg_id, project_ident], row_to_message)?;
    Ok(rows.next().transpose()?)
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        project_ident: row.get(1)?,
        source: row.get(2)?,
        external_message_id: row.get(3)?,
        content: row.get(4)?,
        sent_at: row.get(5)?,
        confirmed_at: row.get(6)?,
        parent_message_id: row.get(7)?,
        agent_id: row.get(8)?,
        message_type: row
            .get::<_, Option<String>>(9)?
            .unwrap_or_else(|| "message".into()),
        subject: row.get(10)?,
        hostname: row.get(11)?,
        event_at: row.get(12)?,
    })
}

// ── Retention ─────────────────────────────────────────────────────────────────

pub fn purge_old_messages(conn: &Connection, cutoff_ms: i64) -> Result<usize> {
    // agent_confirmations cleaned up via ON DELETE CASCADE on messages(id).
    let n = conn.execute(
        "DELETE FROM messages
         WHERE sent_at < ?1
           AND confirmed_at IS NOT NULL",
        params![cutoff_ms],
    )?;
    Ok(n)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ── Dashboard ─────────────────────────────────────────────────────────────────

#[derive(serde::Serialize)]
pub struct ProjectStats {
    pub ident: String,
    pub channel_name: String,
    pub room_id: String,
    pub total_messages: i64,
    pub unread_count: i64,
}

#[derive(serde::Serialize)]
pub struct DashboardData {
    pub project_count: i64,
    pub total_messages: i64,
    pub agent_messages: i64,
    pub user_messages: i64,
    pub skill_count: i64,
    pub projects: Vec<ProjectStats>,
}

/// Return per-project stats ordered by most-recently-created first.
///
/// Shared by the HTML dashboard (via [`get_dashboard_data`]) and the JSON
/// helper endpoint the task picker binds to. Each row contains the project's
/// identity, its channel, the originating room id, the total message count,
/// and the number of unconfirmed user-sourced messages.
pub fn list_project_stats(conn: &Connection) -> Result<Vec<ProjectStats>> {
    let mut stmt = conn.prepare_cached(
        "SELECT p.ident, p.channel_name, p.room_id,
                COUNT(m.id),
                (SELECT COUNT(*) FROM messages m2
                 WHERE m2.project_ident = p.ident
                   AND m2.confirmed_at IS NULL
                   AND m2.source = 'user')
         FROM projects p
         LEFT JOIN messages m ON m.project_ident = p.ident
         GROUP BY p.ident
         ORDER BY p.created_at DESC",
    )?;
    let projects = stmt
        .query_map([], |r| {
            Ok(ProjectStats {
                ident: r.get(0)?,
                channel_name: r.get(1)?,
                room_id: r.get(2)?,
                total_messages: r.get(3)?,
                unread_count: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(projects)
}

pub fn get_dashboard_data(conn: &Connection) -> Result<DashboardData> {
    let project_count: i64 = conn.query_row("SELECT COUNT(*) FROM projects", [], |r| r.get(0))?;
    let total_messages: i64 = conn.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?;
    let agent_messages: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE source='agent'",
        [],
        |r| r.get(0),
    )?;
    let user_messages: i64 = conn.query_row(
        "SELECT COUNT(*) FROM messages WHERE source='user'",
        [],
        |r| r.get(0),
    )?;
    let skill_count: i64 = conn.query_row("SELECT COUNT(*) FROM skills", [], |r| r.get(0))?;

    let projects = list_project_stats(conn)?;

    Ok(DashboardData {
        project_count,
        total_messages,
        agent_messages,
        user_messages,
        skill_count,
        projects,
    })
}

// ── Skills ────────────────────────────────────────────────────────────────────

pub struct SkillRecord {
    pub name: String,
    /// "skill", "command", or "agent"
    pub kind: String,
    pub zip_data: Vec<u8>,
    /// Raw markdown content for commands; None for skills.
    pub content: Option<String>,
    pub size: i64,
    pub checksum: String,
    pub uploaded_at: i64,
}

#[derive(serde::Serialize)]
pub struct SkillMeta {
    pub name: String,
    pub kind: String,
    pub size: i64,
    pub checksum: String,
    pub uploaded_at: i64,
}

pub fn upsert_skill(conn: &Connection, r: &SkillRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO skills (name, kind, zip_data, content, size, checksum, uploaded_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(name) DO UPDATE SET
             kind = excluded.kind,
             zip_data = excluded.zip_data,
             content = excluded.content,
             size = excluded.size,
             checksum = excluded.checksum,
             uploaded_at = excluded.uploaded_at",
        params![
            r.name,
            r.kind,
            r.zip_data,
            r.content,
            r.size,
            r.checksum,
            r.uploaded_at
        ],
    )?;
    Ok(())
}

pub fn get_skill(conn: &Connection, name: &str) -> Result<Option<SkillRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, kind, zip_data, content, size, checksum, uploaded_at FROM skills WHERE name = ?1",
    )?;
    let mut rows = stmt.query_map(params![name], |r| {
        Ok(SkillRecord {
            name: r.get(0)?,
            kind: r.get(1)?,
            zip_data: r.get(2)?,
            content: r.get(3)?,
            size: r.get(4)?,
            checksum: r.get(5)?,
            uploaded_at: r.get(6)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

/// List skill/command/agent metadata. Pass `Some(kind)` to restrict to a
/// single kind (`"skill" | "command" | "agent"`); `None` returns everything.
pub fn list_skills(conn: &Connection, kind: Option<&str>) -> Result<Vec<SkillMeta>> {
    let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<SkillMeta> {
        Ok(SkillMeta {
            name: r.get(0)?,
            kind: r.get(1)?,
            size: r.get(2)?,
            checksum: r.get(3)?,
            uploaded_at: r.get(4)?,
        })
    };

    let collected = match kind {
        Some(k) => {
            let mut stmt = conn.prepare_cached(
                "SELECT name, kind, size, checksum, uploaded_at
                 FROM skills
                 WHERE kind = ?1
                 ORDER BY name ASC",
            )?;
            let rows = stmt
                .query_map(params![k], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
        None => {
            let mut stmt = conn.prepare_cached(
                "SELECT name, kind, size, checksum, uploaded_at
                 FROM skills
                 ORDER BY kind ASC, name ASC",
            )?;
            let rows = stmt
                .query_map([], map_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
    };
    Ok(collected)
}

pub fn delete_skill(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM skills WHERE name = ?1", params![name])?;
    Ok(n > 0)
}

// ── Tasks ─────────────────────────────────────────────────────────────────────

/// How long an `in_progress` task with no activity before it is considered
/// abandoned and returned to `todo` by `reclaim_stale_tasks`.
pub const TASK_RECLAIM_MS: i64 = 60 * 60 * 1000; // 1 hour
/// How long a `done` task remains in the default list view before it falls off.
pub const TASK_DONE_FALLOFF_MS: i64 = 7 * 24 * 60 * 60 * 1000; // 7 days

#[derive(Debug, Clone, serde::Serialize)]
pub struct Task {
    pub id: String,
    pub project_ident: String,
    pub title: String,
    pub description: Option<String>,
    pub details: Option<String>,
    /// One of `"todo"`, `"in_progress"`, `"done"`.
    pub status: String,
    pub rank: i64,
    /// Parsed from the JSON-array-encoded `labels` column; empty when the
    /// column is NULL or malformed.
    pub labels: Vec<String>,
    pub hostname: Option<String>,
    pub owner_agent_id: Option<String>,
    pub reporter: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub started_at: Option<i64>,
    pub done_at: Option<i64>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskComment {
    pub id: String,
    pub task_id: String,
    pub author: String,
    /// One of `"agent"`, `"user"`, `"system"`.
    pub author_type: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskSummary {
    pub id: String,
    pub title: String,
    pub status: String,
    pub rank: i64,
    pub labels: Vec<String>,
    pub owner_agent_id: Option<String>,
    pub hostname: Option<String>,
    pub reporter: String,
    pub comment_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TaskDetail {
    pub task: Task,
    pub comments: Vec<TaskComment>,
}

/// Dynamic update payload for `update_task`. `None` on any field means
/// "do not touch"; for the `Option<Option<_>>` fields, `Some(None)` means
/// "clear the column" and `Some(Some(x))` means "set to x".
pub struct TaskUpdate<'a> {
    pub status: Option<&'a str>,
    pub owner_agent_id: Option<Option<&'a str>>,
    pub rank: Option<i64>,
    pub title: Option<&'a str>,
    pub description: Option<Option<&'a str>>,
    pub details: Option<Option<&'a str>>,
    pub labels: Option<&'a [String]>,
    pub hostname: Option<Option<&'a str>>,
}

fn new_uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn serialize_labels(labels: &[String]) -> Option<String> {
    if labels.is_empty() {
        None
    } else {
        serde_json::to_string(labels).ok()
    }
}

fn parse_labels(raw: Option<String>) -> Vec<String> {
    match raw {
        Some(s) => serde_json::from_str::<Vec<String>>(&s).unwrap_or_default(),
        None => Vec::new(),
    }
}

fn row_to_task(row: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: row.get(0)?,
        project_ident: row.get(1)?,
        title: row.get(2)?,
        description: row.get(3)?,
        details: row.get(4)?,
        status: row.get(5)?,
        rank: row.get(6)?,
        labels: parse_labels(row.get::<_, Option<String>>(7)?),
        hostname: row.get(8)?,
        owner_agent_id: row.get(9)?,
        reporter: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        started_at: row.get(13)?,
        done_at: row.get(14)?,
    })
}

const TASK_SELECT_COLS: &str =
    "id, project_ident, title, description, details, status, rank, labels, \
     hostname, owner_agent_id, reporter, created_at, updated_at, started_at, done_at";

/// Reclaim any task in this project that has been `in_progress` for longer than
/// [`TASK_RECLAIM_MS`] without any `updated_at` activity. Reclaimed tasks are
/// flipped back to `todo`, the owner is cleared, and a system comment is
/// appended so the next agent knows to verify prior progress. Runs in a single
/// transaction.
pub fn reclaim_stale_tasks(conn: &Connection, project_ident: &str) -> Result<usize> {
    let now = now_ms();
    let cutoff = now - TASK_RECLAIM_MS;

    let tx = conn.unchecked_transaction()?;
    let stale_ids: Vec<String> = {
        let mut stmt = tx.prepare(
            "SELECT id FROM tasks
             WHERE project_ident = ?1
               AND status = 'in_progress'
               AND started_at IS NOT NULL
               AND started_at < ?2
               AND updated_at < ?2",
        )?;
        let rows = stmt
            .query_map(params![project_ident, cutoff], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    if stale_ids.is_empty() {
        tx.commit()?;
        return Ok(0);
    }

    let comment_body = "Reclaimed after 1h of inactivity. Next agent please \
                        verify prior progress before continuing.";

    for task_id in &stale_ids {
        tx.execute(
            "INSERT INTO task_comments (id, task_id, author, author_type, content, created_at)
             VALUES (?1, ?2, 'system', 'system', ?3, ?4)",
            params![new_uuid(), task_id, comment_body, now],
        )?;
        tx.execute(
            "UPDATE tasks
             SET status = 'todo',
                 owner_agent_id = NULL,
                 started_at = NULL,
                 updated_at = ?1
             WHERE id = ?2",
            params![now, task_id],
        )?;
    }

    tx.commit()?;
    Ok(stale_ids.len())
}

/// Insert a new task in the `todo` column. Rank is auto-assigned as
/// `MAX(rank) + 1` among existing `todo` rows for this project.
#[allow(clippy::too_many_arguments)]
pub fn insert_task(
    conn: &Connection,
    project_ident: &str,
    title: &str,
    description: Option<&str>,
    details: Option<&str>,
    labels: &[String],
    hostname: Option<&str>,
    reporter: &str,
) -> Result<Task> {
    let id = new_uuid();
    let now = now_ms();
    let labels_json = serialize_labels(labels);

    let rank: i64 = conn.query_row(
        "SELECT COALESCE(MAX(rank), 0) + 1 FROM tasks
         WHERE project_ident = ?1 AND status = 'todo'",
        params![project_ident],
        |r| r.get(0),
    )?;

    conn.execute(
        "INSERT INTO tasks (
             id, project_ident, title, description, details, status, rank,
             labels, hostname, owner_agent_id, reporter,
             created_at, updated_at, started_at, done_at
         ) VALUES (?1, ?2, ?3, ?4, ?5, 'todo', ?6, ?7, ?8, NULL, ?9, ?10, ?10, NULL, NULL)",
        params![
            id,
            project_ident,
            title,
            description,
            details,
            rank,
            labels_json,
            hostname,
            reporter,
            now,
        ],
    )?;

    Ok(Task {
        id,
        project_ident: project_ident.to_string(),
        title: title.to_string(),
        description: description.map(str::to_string),
        details: details.map(str::to_string),
        status: "todo".to_string(),
        rank,
        labels: labels.to_vec(),
        hostname: hostname.map(str::to_string),
        owner_agent_id: None,
        reporter: reporter.to_string(),
        created_at: now,
        updated_at: now,
        started_at: None,
        done_at: None,
    })
}

/// List task summaries for a project filtered by status. When `statuses`
/// contains `"done"` and `include_stale_done` is false, only done tasks whose
/// `done_at > now - 7d` are returned. Results are sorted by `rank ASC` then
/// `updated_at DESC`.
pub fn list_tasks(
    conn: &Connection,
    project_ident: &str,
    statuses: &[String],
    include_stale_done: bool,
) -> Result<Vec<TaskSummary>> {
    if statuses.is_empty() {
        return Ok(Vec::new());
    }

    // Build an IN (?, ?, ...) clause. Placeholders start at 2 because
    // placeholder 1 is project_ident. A trailing `done_cutoff` placeholder is
    // appended when we need to filter stale-done rows.
    let mut placeholders = Vec::with_capacity(statuses.len());
    for i in 0..statuses.len() {
        placeholders.push(format!("?{}", i + 2));
    }
    let in_clause = placeholders.join(",");

    let has_done = statuses.iter().any(|s| s == "done");
    let apply_done_filter = has_done && !include_stale_done;

    let sql = if apply_done_filter {
        let cutoff_ph = statuses.len() + 2;
        format!(
            "SELECT t.id, t.title, t.status, t.rank, t.labels,
                    t.owner_agent_id, t.hostname, t.reporter,
                    (SELECT COUNT(*) FROM task_comments tc WHERE tc.task_id = t.id),
                    t.created_at, t.updated_at
             FROM tasks t
             WHERE t.project_ident = ?1
               AND t.status IN ({in_clause})
               AND (t.status != 'done' OR (t.done_at IS NOT NULL AND t.done_at > ?{cutoff_ph}))
             ORDER BY t.rank ASC, t.updated_at DESC"
        )
    } else {
        format!(
            "SELECT t.id, t.title, t.status, t.rank, t.labels,
                    t.owner_agent_id, t.hostname, t.reporter,
                    (SELECT COUNT(*) FROM task_comments tc WHERE tc.task_id = t.id),
                    t.created_at, t.updated_at
             FROM tasks t
             WHERE t.project_ident = ?1
               AND t.status IN ({in_clause})
             ORDER BY t.rank ASC, t.updated_at DESC"
        )
    };

    // Bind params: project_ident, statuses..., [done_cutoff]
    let mut bound: Vec<Box<dyn rusqlite::ToSql>> = Vec::with_capacity(statuses.len() + 2);
    bound.push(Box::new(project_ident.to_string()));
    for s in statuses {
        bound.push(Box::new(s.clone()));
    }
    if apply_done_filter {
        bound.push(Box::new(now_ms() - TASK_DONE_FALLOFF_MS));
    }
    let params_vec: Vec<&dyn rusqlite::ToSql> = bound.iter().map(|b| b.as_ref()).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_vec.as_slice(), |r| {
            Ok(TaskSummary {
                id: r.get(0)?,
                title: r.get(1)?,
                status: r.get(2)?,
                rank: r.get(3)?,
                labels: parse_labels(r.get::<_, Option<String>>(4)?),
                owner_agent_id: r.get(5)?,
                hostname: r.get(6)?,
                reporter: r.get(7)?,
                comment_count: r.get(8)?,
                created_at: r.get(9)?,
                updated_at: r.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows)
}

/// Fetch a task and all of its comments, scoped by `project_ident` for safety.
/// Comments are ordered ascending by `created_at`.
pub fn get_task_detail(
    conn: &Connection,
    project_ident: &str,
    task_id: &str,
) -> Result<Option<TaskDetail>> {
    let task = {
        let sql =
            format!("SELECT {TASK_SELECT_COLS} FROM tasks WHERE id = ?1 AND project_ident = ?2");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![task_id, project_ident], row_to_task)?;
        match rows.next() {
            Some(r) => r?,
            None => return Ok(None),
        }
    };

    let mut stmt = conn.prepare_cached(
        "SELECT id, task_id, author, author_type, content, created_at
         FROM task_comments
         WHERE task_id = ?1
         ORDER BY created_at ASC",
    )?;
    let comments = stmt
        .query_map(params![task_id], |r| {
            Ok(TaskComment {
                id: r.get(0)?,
                task_id: r.get(1)?,
                author: r.get(2)?,
                author_type: r.get(3)?,
                content: r.get(4)?,
                created_at: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(Some(TaskDetail { task, comments }))
}

/// Apply a partial update to a task, enforcing status-transition side effects
/// (auto-set `started_at`/`owner_agent_id` on todo→in_progress, `done_at` on
/// `* → done`, clearing of timestamps on reverse transitions, etc.). Returns
/// the refreshed task, or `Ok(None)` if no such task exists in that project.
/// Invalid status strings or impossible transitions bubble up via
/// `anyhow::bail!`.
pub fn update_task(
    conn: &Connection,
    project_ident: &str,
    task_id: &str,
    upd: &TaskUpdate<'_>,
    actor_agent_id: Option<&str>,
) -> Result<Option<Task>> {
    // Validate requested status early.
    if let Some(s) = upd.status {
        if s != "todo" && s != "in_progress" && s != "done" {
            anyhow::bail!("invalid status '{s}': must be todo|in_progress|done");
        }
    }

    // Load current state scoped to project for safety.
    let current = {
        let sql =
            format!("SELECT {TASK_SELECT_COLS} FROM tasks WHERE id = ?1 AND project_ident = ?2");
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query_map(params![task_id, project_ident], row_to_task)?;
        match rows.next() {
            Some(r) => r?,
            None => return Ok(None),
        }
    };

    let now = now_ms();
    let new_status = upd.status.unwrap_or(&current.status).to_string();
    let owner_explicit = upd.owner_agent_id.is_some();
    let transitioning = upd.status.is_some() && new_status != current.status;

    // Compute derived fields based on the transition.
    // started_at: Some(Some(v)) = set, Some(None) = clear, None = leave alone
    #[allow(clippy::type_complexity)]
    let mut started_at: Option<Option<i64>> = None;
    let mut done_at: Option<Option<i64>> = None;
    // owner: mirrors TaskUpdate's owner semantics, starting from upd.owner_agent_id.
    let mut owner: Option<Option<String>> =
        upd.owner_agent_id.map(|inner| inner.map(|s| s.to_string()));

    if transitioning {
        match (current.status.as_str(), new_status.as_str()) {
            ("todo", "in_progress") => {
                started_at = Some(Some(now));
                done_at = Some(None);
                if !owner_explicit {
                    if let Some(aid) = actor_agent_id {
                        owner = Some(Some(aid.to_string()));
                    }
                }
            }
            ("in_progress", "todo") => {
                started_at = Some(None);
                if !owner_explicit {
                    owner = Some(None);
                }
            }
            (_, "done") => {
                done_at = Some(Some(now));
            }
            ("done", "todo") | ("done", "in_progress") => {
                done_at = Some(None);
                if new_status == "in_progress" {
                    started_at = Some(Some(now));
                    if !owner_explicit {
                        if let Some(aid) = actor_agent_id {
                            owner = Some(Some(aid.to_string()));
                        }
                    }
                }
            }
            _ => {}
        }
    }

    // Build dynamic UPDATE.
    let mut sets: Vec<String> = Vec::new();
    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    let push = |col: &str,
                val: Box<dyn rusqlite::ToSql>,
                sets: &mut Vec<String>,
                binds: &mut Vec<Box<dyn rusqlite::ToSql>>| {
        binds.push(val);
        sets.push(format!("{col} = ?{}", binds.len()));
    };

    if let Some(title) = upd.title {
        push("title", Box::new(title.to_string()), &mut sets, &mut binds);
    }
    if let Some(desc) = upd.description {
        push(
            "description",
            Box::new(desc.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(det) = upd.details {
        push(
            "details",
            Box::new(det.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(labels) = upd.labels {
        push(
            "labels",
            Box::new(serialize_labels(labels)),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(host) = upd.hostname {
        push(
            "hostname",
            Box::new(host.map(str::to_string)),
            &mut sets,
            &mut binds,
        );
    }
    if upd.status.is_some() {
        push(
            "status",
            Box::new(new_status.clone()),
            &mut sets,
            &mut binds,
        );
    }
    if let Some(rank) = upd.rank {
        push("rank", Box::new(rank), &mut sets, &mut binds);
    }
    if let Some(owner_val) = owner {
        push("owner_agent_id", Box::new(owner_val), &mut sets, &mut binds);
    }
    if let Some(started) = started_at {
        push("started_at", Box::new(started), &mut sets, &mut binds);
    }
    if let Some(done) = done_at {
        push("done_at", Box::new(done), &mut sets, &mut binds);
    }

    // Always bump updated_at.
    push("updated_at", Box::new(now), &mut sets, &mut binds);

    if sets.is_empty() {
        // Nothing to update — just return the current row.
        return Ok(Some(current));
    }

    // WHERE bindings come last.
    binds.push(Box::new(task_id.to_string()));
    let id_ph = binds.len();
    binds.push(Box::new(project_ident.to_string()));
    let proj_ph = binds.len();

    let sql = format!(
        "UPDATE tasks SET {} WHERE id = ?{} AND project_ident = ?{}",
        sets.join(", "),
        id_ph,
        proj_ph,
    );

    let params_vec: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();
    let n = conn.execute(&sql, params_vec.as_slice())?;
    if n == 0 {
        return Ok(None);
    }

    // Re-read the row to return a fully-consistent Task.
    let sql = format!("SELECT {TASK_SELECT_COLS} FROM tasks WHERE id = ?1 AND project_ident = ?2");
    let mut stmt = conn.prepare(&sql)?;
    let mut rows = stmt.query_map(params![task_id, project_ident], row_to_task)?;
    Ok(rows.next().transpose()?)
}

/// Append a comment to a task and bump the parent task's `updated_at`. Runs
/// inside a single transaction. `author_type` must be `"agent"`, `"user"`, or
/// `"system"`.
pub fn insert_comment(
    conn: &Connection,
    task_id: &str,
    author: &str,
    author_type: &str,
    content: &str,
) -> Result<TaskComment> {
    if author_type != "agent" && author_type != "user" && author_type != "system" {
        anyhow::bail!("invalid author_type '{author_type}': must be agent|user|system");
    }

    let id = new_uuid();
    let now = now_ms();

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO task_comments (id, task_id, author, author_type, content, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![id, task_id, author, author_type, content, now],
    )?;
    tx.execute(
        "UPDATE tasks SET updated_at = ?1 WHERE id = ?2",
        params![now, task_id],
    )?;
    tx.commit()?;

    Ok(TaskComment {
        id,
        task_id: task_id.to_string(),
        author: author.to_string(),
        author_type: author_type.to_string(),
        content: content.to_string(),
        created_at: now,
    })
}

/// Delete a task (and its comments via ON DELETE CASCADE) scoped to a project.
/// Returns true if a row was removed.
pub fn delete_task(conn: &Connection, project_ident: &str, task_id: &str) -> Result<bool> {
    let n = conn.execute(
        "DELETE FROM tasks WHERE id = ?1 AND project_ident = ?2",
        params![task_id, project_ident],
    )?;
    Ok(n > 0)
}

/// Current row snapshot read at the top of `reorder_tasks_in_column`:
/// `(status, owner_agent_id, started_at, done_at)`.
type TaskStateSnapshot = (String, Option<String>, Option<i64>, Option<i64>);

/// Apply a client-driven order to one status column. For each id in `order`,
/// set `status = target_status` and `rank = index` (0-based). Any status
/// transition also maintains the invariants enforced by [`update_task`]:
///
/// - transitioning into `in_progress` sets `started_at = now`, auto-assigns
///   `owner_agent_id = actor_agent_id` (when provided and the current owner
///   is `NULL`),
/// - transitioning into `done` sets `done_at = now`,
/// - transitioning out of `done` clears `done_at`,
/// - transitioning out of `in_progress` clears `started_at` and clears
///   `owner_agent_id`.
///
/// All writes happen inside a single transaction; any id that does not exist
/// in this project causes the whole batch to be rolled back via
/// `anyhow::bail!`.
pub fn reorder_tasks_in_column(
    conn: &Connection,
    project_ident: &str,
    target_status: &str,
    order: &[String],
    actor_agent_id: Option<&str>,
) -> Result<()> {
    if target_status != "todo" && target_status != "in_progress" && target_status != "done" {
        anyhow::bail!("invalid status '{target_status}': must be todo|in_progress|done");
    }

    if order.is_empty() {
        return Ok(());
    }

    let now = now_ms();
    let tx = conn.unchecked_transaction()?;

    for (idx, task_id) in order.iter().enumerate() {
        // Fetch current status + owner for this row, scoped to the project.
        let current: Option<TaskStateSnapshot> = {
            let mut stmt = tx.prepare(
                "SELECT status, owner_agent_id, started_at, done_at
                 FROM tasks WHERE id = ?1 AND project_ident = ?2",
            )?;
            let mut rows = stmt.query_map(params![task_id, project_ident], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?,
                    r.get::<_, Option<i64>>(2)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            })?;
            rows.next().transpose()?
        };

        let (old_status, old_owner, old_started_at, old_done_at) = match current {
            Some(v) => v,
            None => anyhow::bail!("task '{task_id}' not found in project '{project_ident}'"),
        };

        // Compute new timestamps + owner by mirroring the transition logic
        // in `update_task`. Fields we do not change are preserved explicitly
        // (not cleared) — this handler only touches status/rank/timestamps.
        let mut new_started_at = old_started_at;
        let mut new_done_at = old_done_at;
        let mut new_owner = old_owner.clone();

        if old_status != target_status {
            match (old_status.as_str(), target_status) {
                ("todo", "in_progress") => {
                    new_started_at = Some(now);
                    new_done_at = None;
                    if new_owner.is_none() {
                        if let Some(aid) = actor_agent_id {
                            new_owner = Some(aid.to_string());
                        }
                    }
                }
                ("in_progress", "todo") => {
                    new_started_at = None;
                    new_owner = None;
                }
                ("in_progress", "done") => {
                    new_done_at = Some(now);
                    // started_at stays put as a historical record of the
                    // in_progress window; `update_task` behaves the same way
                    // when transitioning directly to done.
                }
                ("todo", "done") => {
                    new_done_at = Some(now);
                }
                ("done", "todo") => {
                    new_done_at = None;
                }
                ("done", "in_progress") => {
                    new_done_at = None;
                    new_started_at = Some(now);
                    if new_owner.is_none() {
                        if let Some(aid) = actor_agent_id {
                            new_owner = Some(aid.to_string());
                        }
                    }
                }
                _ => {}
            }
        }

        tx.execute(
            "UPDATE tasks
             SET status = ?1,
                 rank = ?2,
                 started_at = ?3,
                 done_at = ?4,
                 owner_agent_id = ?5,
                 updated_at = ?6
             WHERE id = ?7 AND project_ident = ?8",
            params![
                target_status,
                idx as i64,
                new_started_at,
                new_done_at,
                new_owner,
                now,
                task_id,
                project_ident,
            ],
        )?;
    }

    tx.commit()?;
    Ok(())
}
