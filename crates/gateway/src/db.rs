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

pub struct ProjectStats {
    pub ident: String,
    pub channel_name: String,
    pub room_id: String,
    pub total_messages: i64,
    pub unread_count: i64,
}

pub struct DashboardData {
    pub project_count: i64,
    pub total_messages: i64,
    pub agent_messages: i64,
    pub user_messages: i64,
    pub skill_count: i64,
    pub projects: Vec<ProjectStats>,
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

pub fn list_skills(conn: &Connection) -> Result<Vec<SkillMeta>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, kind, size, checksum, uploaded_at FROM skills ORDER BY kind ASC, name ASC",
    )?;
    let collected = stmt
        .query_map([], |r| {
            Ok(SkillMeta {
                name: r.get(0)?,
                kind: r.get(1)?,
                size: r.get(2)?,
                checksum: r.get(3)?,
                uploaded_at: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(collected)
}

pub fn delete_skill(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM skills WHERE name = ?1", params![name])?;
    Ok(n > 0)
}
