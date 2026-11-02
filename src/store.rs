//! Conversation and audit store owned by the kernel.
//!
//! Domain data stays in the site and CRM. This database holds only
//! conversations, messages, tool-call traces and timestamps.

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

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id, created_at)",
    )
    .execute(&pool)
    .await?;

    Ok(pool)
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
            "INSERT INTO conversations (id, channel, actor_id, actor_role, data_class, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(channel)
        .bind(actor_id)
        .bind(actor_role)
        .bind(data_class)
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
            created_at: now.clone(),
            updated_at: now,
        });
    }

    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO conversations (id, channel, actor_id, actor_role, data_class, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(channel)
    .bind(actor_id)
    .bind(actor_role)
    .bind(data_class)
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
        created_at: now.clone(),
        updated_at: now,
    })
}

pub async fn find_conversation(
    pool: &SqlitePool,
    id: &str,
) -> ApiResult<Option<Conversation>> {
    let row = sqlx::query(
        "SELECT id, channel, actor_id, actor_role, data_class, created_at, updated_at
         FROM conversations WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;

    Ok(row.map(|row| Conversation {
        id: row.get("id"),
        channel: row.get("channel"),
        actor_id: row.get("actor_id"),
        actor_role: row.get("actor_role"),
        data_class: row.get("data_class"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }))
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
) -> ApiResult<StoredMessage> {
    let id = Uuid::new_v4().to_string();
    let now = Utc::now().to_rfc3339();
    let tool_calls_json = tool_calls.map(|value| value.to_string());

    sqlx::query(
        "INSERT INTO messages (id, conversation_id, role, content, tool_calls, tool_call_id, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(conversation_id)
    .bind(role)
    .bind(content)
    .bind(&tool_calls_json)
    .bind(tool_call_id)
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
        created_at: now,
    })
}

pub async fn load_messages(
    pool: &SqlitePool,
    conversation_id: &str,
    limit: i64,
) -> ApiResult<Vec<StoredMessage>> {
    let rows = sqlx::query(
        "SELECT id, role, content, tool_calls, tool_call_id, created_at
         FROM messages WHERE conversation_id = ?
         ORDER BY rowid DESC LIMIT ?",
    )
    .bind(conversation_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;

    let mut messages: Vec<StoredMessage> = rows
        .into_iter()
        .map(|row| StoredMessage {
            id: row.get("id"),
            role: row.get("role"),
            content: row.get("content"),
            tool_calls: row
                .get::<Option<String>, _>("tool_calls")
                .and_then(|raw| serde_json::from_str(&raw).ok()),
            tool_call_id: row.get("tool_call_id"),
            created_at: row.get("created_at"),
        })
        .collect();
    messages.reverse();
    Ok(messages)
}

pub async fn delete_conversation(pool: &SqlitePool, id: &str) -> ApiResult<()> {
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}
