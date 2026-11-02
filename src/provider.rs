//! Channel provider client.
//!
//! A provider is a domain service (site or CRM). It exposes a minimal internal
//! contract:
//!
//! * `GET  {base}/profile?actor_id=...&actor_role=...`
//! * `POST {base}/tools/{tool}`
//!
//! The kernel never receives database credentials. It receives tool schemas
//! and calls typed tools. The provider enforces its own authorization and
//! confirmation rules.

use std::time::Duration;

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
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("provider HTTP client must build");
        Self { config, client }
    }

    pub fn is_configured(&self) -> bool {
        self.config.is_configured()
    }

    pub async fn profile(&self, channel: &str, actor: &Actor) -> ApiResult<Profile> {
        let response = self
            .client
            .get(format!("{}/profile", self.config.base_url))
            .query(&[("actor_id", actor.id.as_str()), ("actor_role", actor.role.as_str())])
            .bearer_auth(&self.config.token)
            .header("x-kernel-channel", channel)
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
        tools.truncate(32);

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
    ) -> ApiResult<ToolOutcome> {
        if !valid_tool_name(tool) {
            return Err(ApiError::bad_request("invalid tool name"));
        }

        let response = self
            .client
            .post(format!("{}/tools/{}", self.config.base_url, tool))
            .bearer_auth(&self.config.token)
            .header("x-kernel-channel", channel)
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

fn truncate(value: &str) -> String {
    value.chars().take(240).collect()
}
