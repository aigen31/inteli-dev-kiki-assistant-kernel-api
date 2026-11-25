//! Client for the site-study sidecar that wraps DeepSeek Harness.
//!
//! The sidecar owns process execution, workspaces and event capture. The
//! kernel only submits a task and polls its result, so a long site study never
//! blocks a chat request.

use std::time::Duration;

use reqwest::Client;
use serde_json::{json, Value};

use crate::config::HarnessConfig;
use crate::error::{ApiError, ApiResult};

#[derive(Clone)]
pub struct HarnessClient {
    config: HarnessConfig,
    client: Client,
}

impl HarnessClient {
    pub fn new(config: HarnessConfig) -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .expect("harness HTTP client must build");
        Self { config, client }
    }

    pub fn is_configured(&self) -> bool {
        self.config.is_configured()
    }

    pub async fn start_run(
        &self,
        task: &str,
        seed: &Value,
        session_id: Option<&str>,
        correlation_id: &str,
    ) -> ApiResult<String> {
        if !self.is_configured() {
            return Err(ApiError::unavailable("site study runner is not configured"));
        }

        let mut payload = json!({
            "task": task,
            "seed": seed,
            "timeout_seconds": self.config.timeout_seconds,
        });

        if let Some(session_id) = session_id {
            payload["session_id"] = Value::String(session_id.to_string());
        }

        let response = self
            .client
            .post(format!("{}/v1/runs", self.config.base_url))
            .bearer_auth(&self.config.token)
            .header("x-correlation-id", correlation_id)
            .json(&payload)
            .send()
            .await
            .map_err(|error| ApiError::upstream(format!("harness start failed: {error}")))?;

        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|error| ApiError::upstream(format!("invalid harness start response: {error}")))?;

        if !status.is_success() {
            return Err(ApiError::upstream(format!(
                "harness start returned {status}: {}",
                body.get("error").and_then(Value::as_str).unwrap_or("unknown")
            )));
        }

        body.get("run_id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| ApiError::upstream("harness start returned no run_id"))
    }

    pub async fn get_run(&self, run_id: &str, correlation_id: &str) -> ApiResult<Value> {
        let response = self
            .client
            .get(format!("{}/v1/runs/{}", self.config.base_url, run_id))
            .bearer_auth(&self.config.token)
            .header("x-correlation-id", correlation_id)
            .send()
            .await
            .map_err(|error| ApiError::upstream(format!("harness status failed: {error}")))?;

        if !response.status().is_success() {
            return Err(ApiError::upstream(format!(
                "harness status returned {}",
                response.status()
            )));
        }

        response
            .json()
            .await
            .map_err(|error| ApiError::upstream(format!("invalid harness status: {error}")))
    }

    pub async fn cancel_run(&self, run_id: &str, correlation_id: &str) -> ApiResult<()> {
        let response = self
            .client
            .post(format!("{}/v1/runs/{}/cancel", self.config.base_url, run_id))
            .bearer_auth(&self.config.token)
            .header("x-correlation-id", correlation_id)
            .send()
            .await
            .map_err(|error| ApiError::upstream(format!("harness cancel failed: {error}")))?;

        if !response.status().is_success() {
            return Err(ApiError::upstream(format!(
                "harness cancel returned {}",
                response.status()
            )));
        }

        Ok(())
    }
}
