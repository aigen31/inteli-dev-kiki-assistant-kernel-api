//! OpenAI-compatible model client with local/cloud routing and a small
//! in-process circuit breaker.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::ModelConfig;
use crate::error::{ApiError, ApiResult};

#[derive(Debug, Clone, Serialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self { role: "system".into(), content: Some(content.into()), tool_calls: None, tool_call_id: None }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Option<Value>) -> Self {
        Self { role: "assistant".into(), content: Some(content.into()), tool_calls, tool_call_id: None }
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self { role: "tool".into(), content: Some(content.into()), tool_calls: None, tool_call_id: Some(tool_call_id.into()) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Route {
    Cloud,
    Local,
}

impl Route {
    pub fn label(self) -> &'static str {
        match self {
            Route::Cloud => "cloud",
            Route::Local => "local",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

#[derive(Debug, Clone)]
pub struct ModelAnswer {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub raw_tool_calls: Option<Value>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

struct BreakerState {
    failures: u32,
    opened_at: Option<Instant>,
}

pub struct ModelClient {
    cloud: ModelConfig,
    local: ModelConfig,
    allow_cloud_for_internal: bool,
    client: Client,
    breakers: Mutex<HashMap<Route, BreakerState>>,
}

const FAILURE_THRESHOLD: u32 = 3;
const OPEN_SECONDS: u64 = 30;

impl ModelClient {
    pub fn new(cloud: ModelConfig, local: ModelConfig, allow_cloud_for_internal: bool) -> Self {
        Self {
            cloud,
            local,
            allow_cloud_for_internal,
            client: Client::builder()
                .timeout(Duration::from_secs(75))
                .build()
                .expect("model HTTP client must build"),
            breakers: Mutex::new(HashMap::new()),
        }
    }

    pub fn route_for(&self, data_class: &str) -> ApiResult<Route> {
        match data_class {
            "public" => {
                if self.cloud.is_configured() {
                    Ok(Route::Cloud)
                } else if self.local.is_configured() {
                    Ok(Route::Local)
                } else {
                    Err(ApiError::unavailable("no language model is configured"))
                }
            }
            "internal" | "secret" => {
                if self.local.is_configured() {
                    Ok(Route::Local)
                } else if self.allow_cloud_for_internal && self.cloud.is_configured() {
                    tracing::warn!(data_class, "local model missing, internal request routed to cloud by explicit config");
                    Ok(Route::Cloud)
                } else {
                    Err(ApiError::unavailable(
                        "local model is required for internal or secret data",
                    ))
                }
            }
            _ => Err(ApiError::bad_request("unknown data_class")),
        }
    }

    pub fn model_name(&self, route: Route) -> &str {
        match route {
            Route::Cloud => &self.cloud.model,
            Route::Local => &self.local.model,
        }
    }

    fn endpoint(&self, route: Route) -> &ModelConfig {
        match route {
            Route::Cloud => &self.cloud,
            Route::Local => &self.local,
        }
    }

    pub async fn complete(
        &self,
        route: Route,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> ApiResult<ModelAnswer> {
        self.ensure_closed(route)?;
        let result = self.complete_inner(route, messages, tools).await;
        match &result {
            Ok(_) => self.record_success(route),
            Err(_) => self.record_failure(route),
        }
        result
    }

    async fn complete_inner(
        &self,
        route: Route,
        messages: &[ChatMessage],
        tools: &[Value],
    ) -> ApiResult<ModelAnswer> {
        let endpoint = self.endpoint(route);
        if !endpoint.is_configured() {
            return Err(ApiError::unavailable(format!("{} model is not configured", route.label())));
        }

        let url = completion_url(&endpoint.base_url);
        let mut payload = json!({
            "model": endpoint.model,
            "messages": messages,
            "temperature": 0.4,
            "max_tokens": 900,
            "stream": false,
        });
        if !tools.is_empty() {
            payload["tools"] = Value::Array(tools.to_vec());
            payload["tool_choice"] = Value::String("auto".into());
        }

        let mut last_error = None;
        for attempt in 0..2 {
            let mut request = self.client.post(&url).json(&payload);
            if !endpoint.api_key.is_empty() {
                request = request.bearer_auth(&endpoint.api_key);
            }
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if status.is_success() {
                        let body: Value = response.json().await.map_err(|error| {
                            ApiError::upstream(format!("invalid model response: {error}"))
                        })?;
                        return parse_response(&body, route);
                    }
                    let body = response.text().await.unwrap_or_default();
                    let detail = format!(
                        "{} model returned {status}: {}",
                        route.label(),
                        body.chars().take(240).collect::<String>()
                    );
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    last_error = Some(ApiError::upstream(detail));
                    if !retryable || attempt == 1 {
                        break;
                    }
                }
                Err(error) => {
                    last_error = Some(ApiError::upstream(format!(
                        "{} model request failed: {error}",
                        route.label()
                    )));
                }
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        Err(last_error.unwrap_or_else(|| ApiError::upstream("model request failed")))
    }

    fn ensure_closed(&self, route: Route) -> ApiResult<()> {
        let mut breakers = self.breakers.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(state) = breakers.get(&route) {
            if let Some(opened_at) = state.opened_at {
                if opened_at.elapsed() < Duration::from_secs(OPEN_SECONDS) {
                    return Err(ApiError::unavailable(format!(
                        "{} model circuit is open",
                        route.label()
                    )));
                }
            }
        }
        Ok(())
    }

    fn record_success(&self, route: Route) {
        let mut breakers = self.breakers.lock().unwrap_or_else(|poison| poison.into_inner());
        breakers.insert(route, BreakerState { failures: 0, opened_at: None });
    }

    fn record_failure(&self, route: Route) {
        let mut breakers = self.breakers.lock().unwrap_or_else(|poison| poison.into_inner());
        let state = breakers.entry(route).or_insert(BreakerState { failures: 0, opened_at: None });
        state.failures += 1;
        if state.failures >= FAILURE_THRESHOLD {
            state.opened_at = Some(Instant::now());
            state.failures = 0;
        }
    }
}

fn completion_url(base: &str) -> String {
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else if base.ends_with("/v1") {
        format!("{base}/chat/completions")
    } else {
        format!("{base}/v1/chat/completions")
    }
}

fn parse_response(body: &Value, route: Route) -> ApiResult<ModelAnswer> {
    let message = body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .ok_or_else(|| ApiError::upstream(format!("{} model returned no message", route.label())))?;

    let content = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();

    let raw_tool_calls = message.get("tool_calls").cloned().filter(|value| value.is_array());
    let mut tool_calls = Vec::new();
    if let Some(Value::Array(calls)) = &raw_tool_calls {
        for call in calls {
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
            let name = call
                .get("function")
                .and_then(|function| function.get("name"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let arguments = call
                .get("function")
                .and_then(|function| function.get("arguments"))
                .map(|value| match value {
                    Value::String(raw) => serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.clone())),
                    other => other.clone(),
                })
                .unwrap_or(Value::Null);
            if !id.is_empty() && !name.is_empty() {
                tool_calls.push(ToolCall { id, name, arguments });
            }
        }
    }

    if content.trim().is_empty() && tool_calls.is_empty() {
        return Err(ApiError::upstream(format!("{} model returned an empty answer", route.label())));
    }

    Ok(ModelAnswer {
        content,
        tool_calls,
        raw_tool_calls,
        prompt_tokens: body
            .get("usage")
            .and_then(|usage| usage.get("prompt_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
        completion_tokens: body
            .get("usage")
            .and_then(|usage| usage.get("completion_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(base_url: &str, model: &str) -> ModelConfig {
        ModelConfig {
            base_url: base_url.to_string(),
            api_key: String::new(),
            model: model.to_string(),
            timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn public_data_prefers_cloud() {
        let models = ModelClient::new(
            endpoint("https://cloud.example/v1", "cloud"),
            endpoint("http://local.example/v1", "local"),
            false,
        );
        assert_eq!(models.route_for("public").unwrap(), Route::Cloud);
    }

    #[test]
    fn internal_data_requires_local() {
        let models = ModelClient::new(
            endpoint("https://cloud.example/v1", "cloud"),
            endpoint("http://local.example/v1", "local"),
            false,
        );
        assert_eq!(models.route_for("internal").unwrap(), Route::Local);
        assert_eq!(models.route_for("secret").unwrap(), Route::Local);
    }

    #[test]
    fn internal_data_does_not_fall_back_to_cloud_by_default() {
        let models = ModelClient::new(
            endpoint("https://cloud.example/v1", "cloud"),
            ModelConfig {
                base_url: String::new(),
                api_key: String::new(),
                model: String::new(),
                timeout: Duration::from_secs(1),
            },
            false,
        );
        assert!(models.route_for("internal").is_err());
    }

    #[test]
    fn completion_url_accepts_base_and_v1() {
        assert_eq!(
            completion_url("https://api.example"),
            "https://api.example/v1/chat/completions"
        );
        assert_eq!(
            completion_url("https://api.example/v1"),
            "https://api.example/v1/chat/completions"
        );
        assert_eq!(
            completion_url("https://api.example/v1/chat/completions"),
            "https://api.example/v1/chat/completions"
        );
    }
}
