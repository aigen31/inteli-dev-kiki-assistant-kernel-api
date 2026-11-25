//! Conversation and audit store owned by the kernel.
//!
//! Domain data stays in the site and CRM. This database holds only
//! conversations, messages, tool-call traces, attachment references and
//! timestamps. Session lifecycle (title, pin, archive) is owned here because
//! the kernel already owns conversation memory; channels mirror it for their
//! own UI and for resilience when the kernel is briefly unavailable.

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::error::{ApiError, ApiResult};

#[derive(Debug, Clone, Serialize)]
pub struct Conversation {
    pub id: String,
    pub channel: String,
    pub actor_id: String,
    pub actor_role: String,
    pub data_class: String,
    pub title: String,
    pub pinned: bool,
    pub archived: bool,
    pub created_at: String,
    pub updated_at: String,
}

/// Sidebar row. `message_count` and `preview` are derived in SQL so a session
/// list stays one query regardless of how long each conversation is.
#[derive(Debug, Clone, Serialize)]
pub struct ConversationSummary {
    pub id: String,
    pub title: String,
    pub pinned: bool,
    pub archived: bool,
    pub data_class: String,
    pub message_count: i64,
    pub preview: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StoredMessage {
    pub id: String,
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Value>,
    /// Tool-by-tool record of how the answer was produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace: Option<Value>,
    pub created_at: String,
}

pub async fn init(database_url: &str) -> Result<SqlitePool, sqlx::Error> {
    let options: SqliteConnectOptions = database_url
        .parse::<SqliteConnectOptions>()?
        .create_if_missing(true)
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        .max_connections(5)
        .connect_with(options)
        .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS conversations (
            id TEXT PRIMARY KEY,
            channel TEXT NOT NULL,
            actor_id TEXT NOT NULL,
            actor_role TEXT NOT NULL,
            data_class TEXT NOT NULL,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )"#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS messages (
            id TEXT PRIMARY KEY,
            conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            tool_calls TEXT,
            tool_call_id TEXT,
            created_at TEXT NOT NULL
        )"#,
    )
    .execute(&pool)
    .await?;

    // The first release predates session metadata and attachments. `CREATE
    // TABLE IF NOT EXISTS` does not evolve an existing database, so each new
    // column is added in place and stays idempotent for fresh installs.
    ensure_column(&pool, "conversations", "title", "TEXT NOT NULL DEFAULT ''").await?;
    ensure_column(&pool, "conversations", "pinned", "INTEGER NOT NULL DEFAULT 0").await?;
    ensure_column(&pool, "conversations", "archived", "INTEGER NOT NULL DEFAULT 0").await?;
    ensure_column(&pool, "messages", "attachments", "TEXT").await?;
    ensure_column(&pool, "messages", "trace", "TEXT").await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS knowledge_cache (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )"#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        r#"CREATE TABLE IF NOT EXISTS studies (
            id TEXT PRIMARY KEY,
            channel TEXT NOT NULL,
            actor_id TEXT NOT NULL,
            url TEXT NOT NULL,
            focus TEXT,
            status TEXT NOT NULL,
            runner_run_id TEXT,
            events TEXT,
            agents_md TEXT,
            notes TEXT,
            error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        )"#,
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_studies_actor ON studies(channel, actor_id, created_at)",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id, created_at)",
    )
    .execute(&pool)
    .await?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_conversations_actor ON conversations(channel, actor_id, updated_at)",
    )
    .execute(&pool)
    .await?;

    backfill_titles(&pool).await?;

    Ok(pool)
}

/// Sessions created before titles existed carry an empty label. Derive it from
/// the first user message once, so the sidebar and the local mirror agree.
async fn backfill_titles(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    let rows = sqlx::query("SELECT id FROM conversations WHERE trim(title) = ''")
        .fetch_all(pool)
        .await?;

    for row in rows {
        let id: String = row.get("id");
        let content = sqlx::query(
            "SELECT content FROM messages
              WHERE conversation_id = ? AND role = 'user'
              ORDER BY rowid ASC LIMIT 1",
        )
        .bind(&id)
        .fetch_optional(pool)
        .await?
        .map(|message| message.get::<String, _>("content"));

        if let Some(content) = content {
            sqlx::query("UPDATE conversations SET title = ? WHERE id = ?")
                .bind(title_for(&content))
                .bind(&id)
                .execute(pool)
                .await?;
        }
    }

    Ok(())
}

pub fn title_for(message: &str) -> String {
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let title: String = collapsed.chars().take(80).collect();
    let title = title.trim().to_string();

    if title.is_empty() {
        "Новый диалог".to_string()
    } else {
        title
    }
}

async fn ensure_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    definition: &str,
) -> Result<(), sqlx::Error> {
    let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
        .fetch_all(pool)
        .await?;
    let exists = rows
        .iter()
        .any(|row| row.get::<String, _>("name") == column);
    if !exists {
        sqlx::query(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn ready(pool: &SqlitePool) -> bool {
    sqlx::query("SELECT 1").fetch_one(pool).await.is_ok()
}

pub async fn get_or_create_conversation(
    pool: &SqlitePool,
    requested_id: Option<String>,
    channel: &str,
    actor_id: &str,
    actor_role: &str,
    data_class: &str,
    title: &str,
) -> ApiResult<Conversation> {
    if let Some(id) = requested_id {
        if id.is_empty()
            || id.len() > 128
            || !id
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        {
            return Err(ApiError::bad_request("invalid conversation_id"));
        }
        if let Some(existing) = find_conversation(pool, &id).await? {
            if existing.channel != channel || existing.actor_id != actor_id {
                return Err(ApiError::forbidden(
                    "conversation belongs to another channel or actor",
                ));
            }
            return Ok(existing);
        }
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO conversations
                (id, channel, actor_id, actor_role, data_class, title, pinned, archived, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, 0, 0, ?, ?)",
        )
        .bind(&id)
        .bind(channel)
        .bind(actor_id)
        .bind(actor_role)
        .bind(data_class)
        .bind(title)
        .bind(&now)
        .bind(&now)
        .execute(pool)
        .await?;
        return Ok(Conversation {
            id,
            channel: channel.to_string(),
            actor_id: actor_id.to_string(),
            actor_role: actor_role.to_string(),
            data_class: data_class.to_string(),
            title: title.to_string(),
            pinned: false,
            archived: false,
            created_at: now.clone(),
            updated_at: now,
        });
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO conversations
            (id, channel, actor_id, actor_role, data_class, title, pinned, archived, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, 0, 0, ?, ?)",
    )
    .bind(&id)
    .bind(channel)
    .bind(actor_id)
    .bind(actor_role)
    .bind(data_class)
    .bind(title)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;

    Ok(Conversation {
        id,
        channel: channel.to_string(),
        actor_id: actor_id.to_string(),
        actor_role: actor_role.to_string(),
        data_class: data_class.to_string(),
        title: title.to_string(),
        pinned: false,
        archived: false,
        created_at: now.clone(),
        updated_at: now,
    })
}

pub async fn find_conversation(pool: &SqlitePool, id: &str) -> ApiResult<Option<Conversation>> {
    let row = sqlx::query(
        "SELECT id, channel, actor_id, actor_role, data_class, title, pinned, archived, created_at, updated_at
         FROM conversations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(conversation_from_row))
}

pub async fn list_conversations(
    pool: &SqlitePool,
    channel: &str,
    actor_id: &str,
    search: Option<&str>,
    include_archived: bool,
    limit: i64,
    offset: i64,
) -> ApiResult<Vec<ConversationSummary>> {
    let search = search.map(str::trim).unwrap_or_default();
    let pattern = format!("%{}%", escape_like(search));

    let rows = sqlx::query(
        r#"SELECT c.id, c.title, c.pinned, c.archived, c.data_class, c.created_at, c.updated_at,
                  (SELECT COUNT(*) FROM messages m
                     WHERE m.conversation_id = c.id AND m.role IN ('user', 'assistant')) AS message_count,
                  COALESCE((SELECT m.content FROM messages m
                     WHERE m.conversation_id = c.id AND m.role IN ('user', 'assistant')
                       AND TRIM(m.content) <> ''
                     ORDER BY m.rowid DESC LIMIT 1), '') AS preview
             FROM conversations c
            WHERE c.channel = ?
              AND c.actor_id = ?
              AND (? = 1 OR c.archived = 0)
              AND (? = ''
                   OR c.title LIKE ? ESCAPE '\'
                   OR EXISTS (SELECT 1 FROM messages s
                                WHERE s.conversation_id = c.id
                                  AND s.role = 'user'
                                  AND s.content LIKE ? ESCAPE '\'))
            ORDER BY c.pinned DESC, c.updated_at DESC
            LIMIT ? OFFSET ?"#,
    )
    .bind(channel)
    .bind(actor_id)
    .bind(if include_archived { 1i64 } else { 0i64 })
    .bind(search)
    .bind(&pattern)
    .bind(&pattern)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(summary_from_row).collect())
}

pub async fn conversation_summary(
    pool: &SqlitePool,
    id: &str,
) -> ApiResult<Option<ConversationSummary>> {
    let row = sqlx::query(
        r#"SELECT c.id, c.title, c.pinned, c.archived, c.data_class, c.created_at, c.updated_at,
                  (SELECT COUNT(*) FROM messages m
                     WHERE m.conversation_id = c.id AND m.role IN ('user', 'assistant')) AS message_count,
                  COALESCE((SELECT m.content FROM messages m
                     WHERE m.conversation_id = c.id AND m.role IN ('user', 'assistant')
                       AND TRIM(m.content) <> ''
                     ORDER BY m.rowid DESC LIMIT 1), '') AS preview
             FROM conversations c WHERE c.id = ?"#,
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(summary_from_row))
}

pub async fn update_conversation(
    pool: &SqlitePool,
    id: &str,
    title: Option<&str>,
    pinned: Option<bool>,
    archived: Option<bool>,
) -> ApiResult<()> {
    if let Some(title) = title {
        sqlx::query("UPDATE conversations SET title = ?, updated_at = ? WHERE id = ?")
            .bind(title)
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(pinned) = pinned {
        sqlx::query("UPDATE conversations SET pinned = ? WHERE id = ?")
            .bind(if pinned { 1i64 } else { 0i64 })
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(archived) = archived {
        sqlx::query("UPDATE conversations SET archived = ? WHERE id = ?")
            .bind(if archived { 1i64 } else { 0i64 })
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn touch_conversation(pool: &SqlitePool, id: &str) -> ApiResult<()> {
    sqlx::query("UPDATE conversations SET updated_at = ? WHERE id = ?")
        .bind(Utc::now().to_rfc3339())
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn insert_message(
    pool: &SqlitePool,
    conversation_id: &str,
    role: &str,
    content: &str,
    tool_calls: Option<&Value>,
    tool_call_id: Option<&str>,
    attachments: Option<&Value>,
) -> ApiResult<StoredMessage> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let tool_calls_json = tool_calls.map(|value| value.to_string());
    let attachments_json = attachments.map(|value| value.to_string());

    sqlx::query(
        "INSERT INTO messages
            (id, conversation_id, role, content, tool_calls, tool_call_id, attachments, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(conversation_id)
    .bind(role)
    .bind(content)
    .bind(&tool_calls_json)
    .bind(tool_call_id)
    .bind(&attachments_json)
    .bind(&now)
    .execute(pool)
    .await?;

    touch_conversation(pool, conversation_id).await?;

    Ok(StoredMessage {
        id,
        role: role.to_string(),
        content: content.to_string(),
        tool_calls: tool_calls.cloned(),
        tool_call_id: tool_call_id.map(str::to_string),
        attachments: attachments.cloned(),
        trace: None,
        created_at: now,
    })
}

pub async fn set_message_trace(pool: &SqlitePool, message_id: &str, trace: &Value) -> ApiResult<()> {
    sqlx::query("UPDATE messages SET trace = ? WHERE id = ?")
        .bind(trace.to_string())
        .bind(message_id)
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn cache_get(pool: &SqlitePool, key: &str) -> ApiResult<Option<(String, String)>> {
    let row = sqlx::query("SELECT value, updated_at FROM knowledge_cache WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(row.map(|row| (row.get("value"), row.get("updated_at"))))
}

pub async fn cache_put(pool: &SqlitePool, key: &str, value: &str) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO knowledge_cache (key, value, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
    )
    .bind(key)
    .bind(value)
    .bind(Utc::now().to_rfc3339())
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn load_messages(
    pool: &SqlitePool,
    conversation_id: &str,
    limit: i64,
) -> ApiResult<Vec<StoredMessage>> {
    let rows = sqlx::query(
        "SELECT id, role, content, tool_calls, tool_call_id, attachments, trace, created_at
         FROM messages WHERE conversation_id = ?
         ORDER BY rowid DESC LIMIT ?",
    )
    .bind(conversation_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut messages: Vec<StoredMessage> = rows.into_iter().map(message_from_row).collect();
    messages.reverse();
    Ok(messages)
}

pub async fn latest_user_message(
    pool: &SqlitePool,
    conversation_id: &str,
) -> ApiResult<Option<StoredMessage>> {
    let row = sqlx::query(
        "SELECT id, role, content, tool_calls, tool_call_id, attachments, trace, created_at
         FROM messages WHERE conversation_id = ? AND role = 'user'
         ORDER BY rowid DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(message_from_row))
}

/// Drop everything the model produced after the last user turn. Used by
/// regenerate so the assistant starts from the same prompt instead of
/// appending a second answer to a failed one.
pub async fn delete_messages_after(
    pool: &SqlitePool,
    conversation_id: &str,
    message_id: &str,
) -> ApiResult<()> {
    sqlx::query(
        "DELETE FROM messages
          WHERE conversation_id = ?
            AND rowid > (SELECT rowid FROM messages WHERE id = ? AND conversation_id = ?)",
    )
    .bind(conversation_id)
    .bind(message_id)
    .bind(conversation_id)
    .execute(pool)
    .await?;

    touch_conversation(pool, conversation_id).await?;
    Ok(())
}

pub async fn delete_conversation(pool: &SqlitePool, id: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct Study {
    pub id: String,
    pub channel: String,
    pub actor_id: String,
    pub url: String,
    pub focus: Option<String>,
    pub status: String,
    pub runner_run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents_md: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

pub async fn insert_study(
    pool: &SqlitePool,
    channel: &str,
    actor_id: &str,
    url: &str,
    focus: Option<&str>,
) -> ApiResult<Study> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();

    sqlx::query(
        "INSERT INTO studies
            (id, channel, actor_id, url, focus, status, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, 'running', ?, ?)",
    )
    .bind(&id)
    .bind(channel)
    .bind(actor_id)
    .bind(url)
    .bind(focus)
    .bind(&now)
    .bind(&now)
    .execute(pool)
    .await?;

    Ok(Study {
        id,
        channel: channel.to_string(),
        actor_id: actor_id.to_string(),
        url: url.to_string(),
        focus: focus.map(str::to_string),
        status: "running".to_string(),
        runner_run_id: None,
        events: None,
        agents_md: None,
        notes: None,
        error: None,
        created_at: now.clone(),
        updated_at: now,
    })
}

#[derive(Default)]
pub struct StudyUpdate<'a> {
    pub status: Option<&'a str>,
    pub runner_run_id: Option<&'a str>,
    pub events: Option<&'a Value>,
    pub agents_md: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub error: Option<&'a str>,
}

pub async fn update_study(pool: &SqlitePool, id: &str, patch: StudyUpdate<'_>) -> ApiResult<()> {
    if let Some(value) = patch.status {
        sqlx::query("UPDATE studies SET status = ?, updated_at = ? WHERE id = ?")
            .bind(value)
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(value) = patch.runner_run_id {
        sqlx::query("UPDATE studies SET runner_run_id = ?, updated_at = ? WHERE id = ?")
            .bind(value)
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(value) = patch.events {
        sqlx::query("UPDATE studies SET events = ?, updated_at = ? WHERE id = ?")
            .bind(value.to_string())
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(value) = patch.agents_md {
        sqlx::query("UPDATE studies SET agents_md = ?, updated_at = ? WHERE id = ?")
            .bind(value)
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(value) = patch.notes {
        sqlx::query("UPDATE studies SET notes = ?, updated_at = ? WHERE id = ?")
            .bind(value)
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(pool)
            .await?;
    }
    if let Some(value) = patch.error {
        sqlx::query("UPDATE studies SET error = ?, updated_at = ? WHERE id = ?")
            .bind(value)
            .bind(Utc::now().to_rfc3339())
            .bind(id)
            .execute(pool)
            .await?;
    }
    Ok(())
}

pub async fn find_study(pool: &SqlitePool, id: &str) -> ApiResult<Option<Study>> {
    let row = sqlx::query(
        "SELECT id, channel, actor_id, url, focus, status, runner_run_id, events, agents_md, notes, error, created_at, updated_at
         FROM studies WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(study_from_row))
}

pub async fn list_studies(
    pool: &SqlitePool,
    channel: &str,
    actor_id: &str,
    limit: i64,
) -> ApiResult<Vec<Study>> {
    let rows = sqlx::query(
        "SELECT id, channel, actor_id, url, focus, status, runner_run_id, events, agents_md, notes, error, created_at, updated_at
         FROM studies WHERE channel = ? AND actor_id = ?
         ORDER BY created_at DESC LIMIT ?",
    )
    .bind(channel)
    .bind(actor_id)
    .bind(limit.clamp(1, 50))
    .fetch_all(pool)
    .await?;

    Ok(rows.into_iter().map(study_from_row).collect())
}

fn study_from_row(row: sqlx::sqlite::SqliteRow) -> Study {
    Study {
        id: row.get("id"),
        channel: row.get("channel"),
        actor_id: row.get("actor_id"),
        url: row.get("url"),
        focus: row.get("focus"),
        status: row.get("status"),
        runner_run_id: row.get("runner_run_id"),
        events: row
            .get::<Option<String>, _>("events")
            .and_then(|raw| serde_json::from_str(&raw).ok()),
        agents_md: row.get("agents_md"),
        notes: row.get("notes"),
        error: row.get("error"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn conversation_from_row(row: sqlx::sqlite::SqliteRow) -> Conversation {
    Conversation {
        id: row.get("id"),
        channel: row.get("channel"),
        actor_id: row.get("actor_id"),
        actor_role: row.get("actor_role"),
        data_class: row.get("data_class"),
        title: row.get("title"),
        pinned: row.get::<i64, _>("pinned") != 0,
        archived: row.get::<i64, _>("archived") != 0,
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn summary_from_row(row: sqlx::sqlite::SqliteRow) -> ConversationSummary {
    let title: String = row.get("title");
    ConversationSummary {
        id: row.get("id"),
        title: if title.trim().is_empty() {
            "Новый диалог".to_string()
        } else {
            title
        },
        pinned: row.get::<i64, _>("pinned") != 0,
        archived: row.get::<i64, _>("archived") != 0,
        data_class: row.get("data_class"),
        message_count: row.get("message_count"),
        preview: row.get("preview"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

fn message_from_row(row: sqlx::sqlite::SqliteRow) -> StoredMessage {
    StoredMessage {
        id: row.get("id"),
        role: row.get("role"),
        content: row.get("content"),
        tool_calls: row
            .get::<Option<String>, _>("tool_calls")
            .and_then(|raw| serde_json::from_str(&raw).ok()),
        tool_call_id: row.get("tool_call_id"),
        attachments: row
            .get::<Option<String>, _>("attachments")
            .and_then(|raw| serde_json::from_str(&raw).ok()),
        trace: row
            .get::<Option<String>, _>("trace")
            .and_then(|raw| serde_json::from_str(&raw).ok()),
        created_at: row.get("created_at"),
    }
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_patterns_escape_wildcards() {
        assert_eq!(escape_like("100%_done"), "100\\%\\_done");
    }

    async fn test_pool() -> (SqlitePool, std::path::PathBuf) {
        let path = std::env::temp_dir().join(format!("kernel-test-{}.db", Uuid::new_v4()));
        let url = format!("sqlite://{}?mode=rwc", path.display());
        let pool = init(&url).await.expect("test database");
        (pool, path)
    }

    #[tokio::test]
    async fn session_lifecycle_round_trip() {
        let (pool, path) = test_pool().await;

        let conversation = get_or_create_conversation(
            &pool,
            None,
            "crm",
            "user-1",
            "admin",
            "internal",
            "Баг на главной",
        )
        .await
        .expect("conversation");

        insert_message(&pool, &conversation.id, "user", "Баг на главной", None, None, None)
            .await
            .expect("user message");
        insert_message(&pool, &conversation.id, "assistant", "Посмотрю", None, None, None)
            .await
            .expect("assistant message");

        let sessions = list_conversations(&pool, "crm", "user-1", None, false, 50, 0)
            .await
            .expect("list");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title, "Баг на главной");
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[0].preview, "Посмотрю");
        assert!(!sessions[0].pinned);

        // Search reaches message bodies, not only titles.
        let found = list_conversations(&pool, "crm", "user-1", Some("главной"), false, 50, 0)
            .await
            .expect("search");
        assert_eq!(found.len(), 1);
        let missing = list_conversations(&pool, "crm", "user-1", Some("счёта"), false, 50, 0)
            .await
            .expect("search miss");
        assert!(missing.is_empty());

        // Another actor never sees the session.
        let foreign = list_conversations(&pool, "crm", "user-2", None, false, 50, 0)
            .await
            .expect("foreign list");
        assert!(foreign.is_empty());

        update_conversation(&pool, &conversation.id, Some("Переименован"), Some(true), None)
            .await
            .expect("update");
        let summary = conversation_summary(&pool, &conversation.id)
            .await
            .expect("summary")
            .expect("exists");
        assert_eq!(summary.title, "Переименован");
        assert!(summary.pinned);

        update_conversation(&pool, &conversation.id, None, None, Some(true))
            .await
            .expect("archive");
        let visible = list_conversations(&pool, "crm", "user-1", None, false, 50, 0)
            .await
            .expect("visible");
        assert!(visible.is_empty(), "archived sessions stay out of the default list");

        delete_conversation(&pool, &conversation.id).await.expect("delete");
        assert!(conversation_summary(&pool, &conversation.id)
            .await
            .expect("summary after delete")
            .is_none());

        drop(pool);
        std::fs::remove_file(path).ok();
    }

    #[tokio::test]
    async fn titles_are_backfilled_from_the_first_user_message() {
        let (pool, path) = test_pool().await;

        let conversation = get_or_create_conversation(
            &pool,
            None,
            "crm",
            "user-1",
            "admin",
            "internal",
            "",
        )
        .await
        .expect("conversation");

        insert_message(
            &pool,
            &conversation.id,
            "user",
            "  Сколько   я заработал\nв этом месяце?  ",
            None,
            None,
            None,
        )
        .await
        .expect("message");

        pool.close().await;

        // Re-opening the store runs the one-time title backfill.
        let reopened = init(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("reopened");

        let summary = conversation_summary(&reopened, &conversation.id)
            .await
            .expect("summary")
            .expect("exists");

        assert_eq!(summary.title, "Сколько я заработал в этом месяце?");

        reopened.close().await;
        std::fs::remove_file(path).ok();
    }

    #[tokio::test]
    async fn regenerate_truncates_only_the_trailing_answer() {        let (pool, path) = test_pool().await;

        let conversation = get_or_create_conversation(
            &pool,
            None,
            "crm",
            "user-1",
            "client",
            "internal",
            "Первый вопрос",
        )
        .await
        .expect("conversation");

        insert_message(&pool, &conversation.id, "user", "Первый вопрос", None, None, None)
            .await
            .expect("first user");
        insert_message(&pool, &conversation.id, "assistant", "Первый ответ", None, None, None)
            .await
            .expect("first assistant");

        let last_user = latest_user_message(&pool, &conversation.id)
            .await
            .expect("latest user")
            .expect("exists");
        delete_messages_after(&pool, &conversation.id, &last_user.id)
            .await
            .expect("truncate");

        let remaining = load_messages(&pool, &conversation.id, 50).await.expect("messages");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].role, "user");
        assert_eq!(remaining[0].content, "Первый вопрос");

        drop(pool);
        std::fs::remove_file(path).ok();
    }
}
