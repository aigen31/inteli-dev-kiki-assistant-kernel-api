//! Channel provider client.
//!
//! A provider is a domain service (site or CRM). It exposes a minimal internal
//! contract:
//!
//! * `GET  {base}/profile?actor_id=...&actor_role=...`
//! * `POST {base}/tools/{tool}`
//! * `GET  {base}/files/{file}?conversation_id=...&actor_id=...`
//!
//! The kernel never receives database credentials. It receives tool schemas
//! and calls typed tools. The provider enforces its own authorization and
//! confirmation rules. Images are pulled on demand through the same contract:
//! the kernel holds the bytes only while building one model request, and the
//! domain keeps ownership of storage and access checks.

use std::time::Duration;

/// Provider tools are small schemas; 64 leaves room for the growing CRM
/// surface without silently dropping tools at the old 32-tool ceiling.
const MAX_PROVIDER_TOOLS: usize = 64;

use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::ProviderConfig;
use crate::error::{ApiError, ApiResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Actor {
    pub id: String,
    pub role: String,
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub system: String,
    pub tools: Vec<Value>,
    pub suggested_next: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct ToolOutcome {
    pub result: Value,
    pub pending_action: Option<Value>,
}

#[derive(Debug, Clone)]
pub struct FetchedFile {
    pub mime: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone)]
pub struct ProviderClient {
    config: ProviderConfig,
    client: Client,
}

#[derive(Debug, Deserialize)]
struct ProfileResponse {
    #[serde(default)]
    system: String,
    #[serde(default)]
    tools: Vec<Value>,
    #[serde(default)]
    suggested_next: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct ToolResponse {
    #[serde(default)]
    result: Value,
    #[serde(default)]
    pending_action: Option<Value>,
}

impl ProviderClient {
    pub fn new(config: ProviderConfig) -> Self {
        // Долгие инструменты (research) держат вызов дольше 10 с, поэтому
        // потолок настраивается; по умолчанию оставляем прежние 10 с.
        let timeout = std::env::var("KERNEL_PROVIDER_TIMEOUT_SECONDS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(10)
            .clamp(5, 180);

        let client = Client::builder()
            .timeout(Duration::from_secs(timeout))
            .build()
            .expect("provider HTTP client must build");
        Self { config, client }
    }

    pub fn is_configured(&self) -> bool {
        self.config.is_configured()
    }

    pub async fn profile(
        &self,
        channel: &str,
        actor: &Actor,
        conversation_id: &str,
        correlation_id: &str,
    ) -> ApiResult<Profile> {
        let response = self
            .client
            .get(format!("{}/profile", self.config.base_url))
            .query(&[
                ("actor_id", actor.id.as_str()),
                ("actor_role", actor.role.as_str()),
                ("conversation_id", conversation_id),
            ])
            .bearer_auth(&self.config.token)
            .header("x-kernel-channel", channel)
            .header("x-correlation-id", correlation_id)
            .send()
            .await
            .map_err(|error| ApiError::upstream(format!("provider profile request failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ApiError::upstream(format!(
                "provider profile returned {status}: {}",
                truncate(&body)
            )));
        }

        let response: ProfileResponse = response
            .json()
            .await
            .map_err(|error| ApiError::upstream(format!("invalid provider profile: {error}")))?;

        let mut tools = response.tools;
        tools.retain(|tool| tool_name(tool).is_some());
        tools.truncate(MAX_PROVIDER_TOOLS);

        Ok(Profile {
            system: response.system,
            tools,
            suggested_next: response.suggested_next,
        })
    }

    pub async fn call_tool(
        &self,
        channel: &str,
        actor: &Actor,
        conversation_id: &str,
        tool: &str,
        arguments: Value,
        correlation_id: &str,
    ) -> ApiResult<ToolOutcome> {
        if !valid_tool_name(tool) {
            return Err(ApiError::bad_request("invalid tool name"));
        }

        let response = self
            .client
            .post(format!("{}/tools/{}", self.config.base_url, tool))
            .bearer_auth(&self.config.token)
            .header("x-kernel-channel", channel)
            .header("x-correlation-id", correlation_id)
            .json(&json!({
                "conversation_id": conversation_id,
                "actor": actor,
                "arguments": arguments,
            }))
            .send()
            .await
            .map_err(|error| ApiError::upstream(format!("provider tool request failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ApiError::upstream(format!(
                "provider tool {tool} returned {status}: {}",
                truncate(&body)
            )));
        }

        let response: ToolResponse = response
            .json()
            .await
            .map_err(|error| ApiError::upstream(format!("invalid provider tool response: {error}")))?;

        Ok(ToolOutcome {
            result: response.result,
            pending_action: response.pending_action,
        })
    }

    /// Read the channel's price list. Only the site owns prices; the kernel
    /// caches the snapshot so every assistant surface shares one answer.
    pub async fn prices(&self, channel: &str, correlation_id: &str) -> ApiResult<Value> {
        let response = self
            .client
            .get(format!("{}/prices", self.config.base_url))
            .bearer_auth(&self.config.token)
            .header("x-kernel-channel", channel)
            .header("x-correlation-id", correlation_id)
            .send()
            .await
            .map_err(|error| ApiError::upstream(format!("provider prices request failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(ApiError::upstream(format!(
                "provider prices returned {status}: {}",
                truncate(&body)
            )));
        }

        response
            .json()
            .await
            .map_err(|error| ApiError::upstream(format!("invalid provider prices: {error}")))
    }

    /// Pull one attachment. Providers decide what a given actor may read; the
    /// kernel only enforces the transport limit and the image MIME allowlist.
    pub async fn fetch_file(
        &self,
        channel: &str,
        actor: &Actor,
        conversation_id: &str,
        file_id: &str,
        max_bytes: usize,
        correlation_id: &str,
    ) -> ApiResult<FetchedFile> {
        if !valid_file_id(file_id) {
            return Err(ApiError::bad_request("invalid attachment id"));
        }

        let response = self
            .client
            .get(format!("{}/files/{}", self.config.base_url, file_id))
            .query(&[
                ("conversation_id", conversation_id),
                ("actor_id", actor.id.as_str()),
            ])
            .bearer_auth(&self.config.token)
            .header("x-kernel-channel", channel)
            .header("x-correlation-id", correlation_id)
            .send()
            .await
            .map_err(|error| ApiError::upstream(format!("provider file request failed: {error}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            let detail = format!("provider file returned {status}: {}", truncate(&body));
            return Err(match status.as_u16() {
                404 => ApiError::not_found(detail),
                413 => ApiError::bad_request("attachment is too large"),
                _ => ApiError::upstream(detail),
            });
        }

        let mime = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .trim()
            .to_string();

        if let Some(length) = response.content_length() {
            if length as usize > max_bytes {
                return Err(ApiError::bad_request(format!(
                    "attachment is larger than {} bytes",
                    max_bytes
                )));
            }
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|error| ApiError::upstream(format!("provider file body failed: {error}")))?;

        if bytes.len() > max_bytes {
            return Err(ApiError::bad_request(format!(
                "attachment is larger than {} bytes",
                max_bytes
            )));
        }

        Ok(FetchedFile { mime, bytes: bytes.to_vec() })
    }
}

pub fn tool_name(tool: &Value) -> Option<String> {
    tool.get("function")
        .and_then(|function| function.get("name"))
        .and_then(Value::as_str)
        .filter(|name| valid_tool_name(name))
        .map(str::to_string)
}

fn valid_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_')
}

pub fn valid_file_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
}

fn truncate(value: &str) -> String {
    value.chars().take(240).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_names_stay_within_the_provider_contract() {
        assert!(valid_tool_name("create_task_from_form"));
        assert!(!valid_tool_name("create-task"));
        assert!(!valid_tool_name(""));
    }

    #[test]
    fn file_ids_reject_path_traversal() {
        assert!(valid_file_id("6f1e7f0e-2a6e-4a5a-8e5c-0f6f0f7a1b2c"));
        assert!(!valid_file_id("../secret"));
        assert!(!valid_file_id("a/b"));
    }
}
