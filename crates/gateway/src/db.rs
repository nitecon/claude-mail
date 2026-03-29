use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use std::sync::{Arc, Mutex};

pub type Db = Arc<Mutex<Connection>>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct Project {
    pub ident: String,
    pub discord_channel_id: String,
    pub last_discord_msg_id: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Message {
    pub id: i64,
    pub project_ident: String,
    pub source: String,
    pub discord_message_id: Option<String>,
    pub content: String,
    pub sent_at: i64,
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
            ident                TEXT PRIMARY KEY,
            discord_channel_id   TEXT NOT NULL,
            last_discord_msg_id  TEXT,
            created_at           INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS messages (
            id                   INTEGER PRIMARY KEY AUTOINCREMENT,
            project_ident        TEXT NOT NULL REFERENCES projects(ident),
            source               TEXT NOT NULL CHECK(source IN ('agent','user')),
            discord_message_id   TEXT,
            content              TEXT NOT NULL,
            sent_at              INTEGER NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_messages_project
            ON messages(project_ident, id);

        CREATE TABLE IF NOT EXISTS cursors (
            project_ident  TEXT PRIMARY KEY REFERENCES projects(ident),
            last_read_id   INTEGER NOT NULL DEFAULT 0,
            updated_at     INTEGER NOT NULL
        );",
    )
    .context("apply schema")
}

// ── Projects ─────────────────────────────────────────────────────────────────

pub fn get_project(conn: &Connection, ident: &str) -> Result<Option<Project>> {
    let mut stmt = conn.prepare_cached(
        "SELECT ident, discord_channel_id, last_discord_msg_id, created_at
         FROM projects WHERE ident = ?1",
    )?;
    let mut rows = stmt.query_map(params![ident], row_to_project)?;
    Ok(rows.next().transpose()?)
}

pub fn insert_project(conn: &Connection, p: &Project) -> Result<()> {
    conn.execute(
        "INSERT INTO projects (ident, discord_channel_id, last_discord_msg_id, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(ident) DO NOTHING",
        params![p.ident, p.discord_channel_id, p.last_discord_msg_id, p.created_at],
    )?;
    // Ensure cursor row exists
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
        "SELECT ident, discord_channel_id, last_discord_msg_id, created_at FROM projects",
    )?;
    let rows = stmt.query_map([], row_to_project)?;
    rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
}

pub fn update_last_discord_msg_id(
    conn: &Connection,
    ident: &str,
    msg_id: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE projects SET last_discord_msg_id = ?1 WHERE ident = ?2",
        params![msg_id, ident],
    )?;
    Ok(())
}

fn row_to_project(row: &rusqlite::Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        ident: row.get(0)?,
        discord_channel_id: row.get(1)?,
        last_discord_msg_id: row.get(2)?,
        created_at: row.get(3)?,
    })
}

// ── Messages ─────────────────────────────────────────────────────────────────

pub fn insert_message(conn: &Connection, m: &Message) -> Result<i64> {
    conn.execute(
        "INSERT INTO messages (project_ident, source, discord_message_id, content, sent_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            m.project_ident,
            m.source,
            m.discord_message_id,
            m.content,
            m.sent_at
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

pub fn update_discord_message_id(conn: &Connection, id: i64, discord_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE messages SET discord_message_id = ?1 WHERE id = ?2",
        params![discord_id, id],
    )?;
    Ok(())
}

/// Returns unread messages and advances the cursor atomically (BEGIN IMMEDIATE).
pub fn get_and_advance_cursor(conn: &Connection, ident: &str) -> Result<Vec<Message>> {
    conn.execute_batch("BEGIN IMMEDIATE")?;

    let cursor: i64 = conn
        .query_row(
            "SELECT last_read_id FROM cursors WHERE project_ident = ?1",
            params![ident],
            |r| r.get(0),
        )
        .unwrap_or(0);

    let msgs: Vec<Message> = {
        let mut stmt = conn.prepare_cached(
            "SELECT id, project_ident, source, discord_message_id, content, sent_at
             FROM messages
             WHERE project_ident = ?1 AND id > ?2
             ORDER BY id ASC",
        )?;
        let collected = stmt
            .query_map(params![ident, cursor], row_to_message)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        collected
    };

    if let Some(last) = msgs.last() {
        let now = now_ms();
        conn.execute(
            "UPDATE cursors SET last_read_id = ?1, updated_at = ?2 WHERE project_ident = ?3",
            params![last.id, now, ident],
        )?;
    }

    conn.execute_batch("COMMIT")?;
    Ok(msgs)
}

pub fn messages_after(
    conn: &Connection,
    ident: &str,
    after_discord_id: &str,
) -> Result<Vec<Message>> {
    // Fetch by discord_message_id ordering (snowflakes are ordered by time).
    // We insert messages where discord_message_id > after_discord_id lexicographically,
    // which works for Discord snowflakes (they are monotonically increasing).
    let mut stmt = conn.prepare_cached(
        "SELECT id, project_ident, source, discord_message_id, content, sent_at
         FROM messages
         WHERE project_ident = ?1
           AND source = 'user'
           AND discord_message_id > ?2
         ORDER BY id ASC",
    )?;
    let collected = stmt
        .query_map(params![ident, after_discord_id], row_to_message)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(collected)
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: row.get(0)?,
        project_ident: row.get(1)?,
        source: row.get(2)?,
        discord_message_id: row.get(3)?,
        content: row.get(4)?,
        sent_at: row.get(5)?,
    })
}

// ── Retention ────────────────────────────────────────────────────────────────

pub fn purge_old_messages(conn: &Connection, cutoff_ms: i64) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM messages
         WHERE sent_at < ?1
           AND id <= (
               SELECT last_read_id FROM cursors
               WHERE project_ident = messages.project_ident
           )",
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
