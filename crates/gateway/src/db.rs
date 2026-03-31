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

    Ok(())
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
    // Agent messages are auto-confirmed at insert time so they never appear in
    // the unconfirmed queue — the agent already knows what it sent.
    let confirmed_at = if m.source == "agent" {
        Some(now_ms())
    } else {
        None
    };
    conn.execute(
        "INSERT INTO messages (project_ident, source, external_message_id, content, sent_at, confirmed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            m.project_ident,
            m.source,
            m.external_message_id,
            m.content,
            m.sent_at,
            confirmed_at
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Returns all unconfirmed messages for a project (peek — no side effects).
pub fn get_unconfirmed_messages(conn: &Connection, ident: &str) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, project_ident, source, external_message_id, content, sent_at, confirmed_at
         FROM messages
         WHERE project_ident = ?1 AND confirmed_at IS NULL
         ORDER BY id ASC",
    )?;
    let collected = stmt
        .query_map(params![ident], row_to_message)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(collected)
}

/// Mark a single message as confirmed. Returns true if the message was unconfirmed and is now confirmed.
pub fn confirm_message(conn: &Connection, project_ident: &str, msg_id: i64) -> Result<bool> {
    let n = conn.execute(
        "UPDATE messages SET confirmed_at = ?1
         WHERE id = ?2 AND project_ident = ?3 AND confirmed_at IS NULL",
        params![now_ms(), msg_id, project_ident],
    )?;
    Ok(n > 0)
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
    })
}

// ── Retention ─────────────────────────────────────────────────────────────────

pub fn purge_old_messages(conn: &Connection, cutoff_ms: i64) -> Result<usize> {
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
    pub zip_data: Vec<u8>,
    pub size: i64,
    pub checksum: String,
    pub uploaded_at: i64,
}

#[derive(serde::Serialize)]
pub struct SkillMeta {
    pub name: String,
    pub size: i64,
    pub checksum: String,
    pub uploaded_at: i64,
}

pub fn upsert_skill(conn: &Connection, r: &SkillRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO skills (name, zip_data, size, checksum, uploaded_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(name) DO UPDATE SET
             zip_data = excluded.zip_data,
             size = excluded.size,
             checksum = excluded.checksum,
             uploaded_at = excluded.uploaded_at",
        params![r.name, r.zip_data, r.size, r.checksum, r.uploaded_at],
    )?;
    Ok(())
}

pub fn get_skill(conn: &Connection, name: &str) -> Result<Option<SkillRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, zip_data, size, checksum, uploaded_at FROM skills WHERE name = ?1",
    )?;
    let mut rows = stmt.query_map(params![name], |r| {
        Ok(SkillRecord {
            name: r.get(0)?,
            zip_data: r.get(1)?,
            size: r.get(2)?,
            checksum: r.get(3)?,
            uploaded_at: r.get(4)?,
        })
    })?;
    Ok(rows.next().transpose()?)
}

pub fn list_skills(conn: &Connection) -> Result<Vec<SkillMeta>> {
    let mut stmt = conn
        .prepare_cached("SELECT name, size, checksum, uploaded_at FROM skills ORDER BY name ASC")?;
    let collected = stmt
        .query_map([], |r| {
            Ok(SkillMeta {
                name: r.get(0)?,
                size: r.get(1)?,
                checksum: r.get(2)?,
                uploaded_at: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(collected)
}

pub fn delete_skill(conn: &Connection, name: &str) -> Result<bool> {
    let n = conn.execute("DELETE FROM skills WHERE name = ?1", params![name])?;
    Ok(n > 0)
}
