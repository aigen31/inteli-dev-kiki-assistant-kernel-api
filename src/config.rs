//! Environment configuration for the assistant kernel.
//!
//! The kernel is a separate deployable. It never reads the site or CRM
//! databases directly. Channel providers are addressed through service-token
//! protected HTTP endpoints, and the kernel owns only conversations, usage and
//! audit data.

use std::env;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct ModelConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout: Duration,
}

impl ModelConfig {
    fn from_env(prefix: &str, default_model: &str) -> Self {
        let base_url = env::var(format!("KERNEL_{prefix}_BASE_URL")).unwrap_or_default();
        let api_key = env::var(format!("KERNEL_{prefix}_API_KEY")).unwrap_or_default();
        let model = env::var(format!("KERNEL_{prefix}_MODEL"))
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| default_model.to_string());
        let timeout = env::var(format!("KERNEL_{prefix}_TIMEOUT_SECONDS"))
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(60);

        Self {
            base_url: base_url.trim().trim_end_matches('/').to_string(),
            api_key,
            model,
            timeout: Duration::from_secs(timeout.max(1)),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.model.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    /// Base URL includes `/internal/assistant`, for example
    /// `http://web:8080/internal/assistant`.
    pub base_url: String,
    /// Token used by the kernel when calling the channel provider.
    pub token: String,
}

impl ProviderConfig {
    fn from_env(prefix: &str) -> Self {
        Self {
            base_url: env::var(format!("KERNEL_{prefix}_PROVIDER_URL"))
                .unwrap_or_default()
                .trim()
                .trim_end_matches('/')
                .to_string(),
            token: env::var(format!("KERNEL_{prefix}_PROVIDER_TOKEN")).unwrap_or_default(),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.token.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub database_url: String,
    pub cloud: ModelConfig,
    pub local: ModelConfig,
    pub site: ProviderConfig,
    pub crm: ProviderConfig,
    /// Inbound token for the site BFF. Empty means the channel is disabled.
    pub site_token: String,
    /// Inbound token for the CRM BFF. Empty means the channel is disabled.
    pub crm_token: String,
    pub max_tool_steps: usize,
    pub max_message_chars: usize,
    /// Completion budget. A plan over a long brief needs room for one large
    /// tool call; 900 was enough for a chat answer but truncated real plans.
    pub max_output_tokens: u64,
    /// How many images one user turn may carry.
    pub max_attachments: usize,
    /// Per-image byte ceiling. Images are proxied through the channel provider,
    /// so the kernel holds bytes in memory only while building the model call.
    pub max_attachment_bytes: usize,
    /// Whether the routed endpoint accepts OpenAI-style `image_url` parts.
    /// Local llama.cpp needs a multimodal projector (`--mmproj`) to do so.
    pub cloud_vision: bool,
    pub local_vision: bool,
    /// Site-study runner (DeepSeek Harness sidecar). Empty disables studies.
    pub harness: HarnessConfig,
    /// How long a fetched price list stays hot in SQLite.
    pub pricing_ttl_seconds: u64,
    /// Never true in production for real secret data. It exists for local
    /// development when the local llama.cpp endpoint is not running yet.
    pub allow_cloud_for_internal: bool,
}

#[derive(Clone, Debug)]
pub struct HarnessConfig {
    pub base_url: String,
    pub token: String,
    pub timeout_seconds: u64,
}

impl HarnessConfig {
    fn from_env() -> Self {
        Self {
            base_url: env::var("KERNEL_HARNESS_URL")
                .unwrap_or_default()
                .trim()
                .trim_end_matches('/')
                .to_string(),
            token: env::var("KERNEL_HARNESS_TOKEN").unwrap_or_default(),
            timeout_seconds: env::var("KERNEL_HARNESS_TIMEOUT_SECONDS")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(300u64)
                .clamp(30, 900),
        }
    }

    pub fn is_configured(&self) -> bool {
        !self.base_url.is_empty() && !self.token.is_empty()
    }
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let bind = env::var("KERNEL_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
        let bind: SocketAddr = bind
            .parse()
            .map_err(|error| format!("KERNEL_BIND is invalid: {error}"))?;

        let database_url = env::var("KERNEL_DATABASE_URL")
            .unwrap_or_else(|_| "sqlite:///data/kernel.db?mode=rwc".to_string());

        let max_tool_steps = env::var("KERNEL_MAX_TOOL_STEPS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8usize)
            .clamp(1, 16);
        let max_message_chars = env::var("KERNEL_MAX_MESSAGE_CHARS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8000usize)
            .clamp(1, 32_000);
        let max_output_tokens = env::var("KERNEL_MAX_OUTPUT_TOKENS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(3000u64)
            .clamp(256, 8000);
        let allow_cloud_for_internal = env::var("KERNEL_ALLOW_CLOUD_FOR_INTERNAL")
            .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        let max_attachments = env::var("KERNEL_MAX_ATTACHMENTS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(4usize)
            .clamp(0, 8);
        let max_attachment_bytes = env::var("KERNEL_MAX_ATTACHMENT_BYTES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(8 * 1024 * 1024)
            .clamp(64 * 1024, 20 * 1024 * 1024);
        let cloud_vision = env::var("KERNEL_CLOUD_VISION")
            .map(|value| !matches!(value.as_str(), "0" | "false" | "no"))
            .unwrap_or(true);
        let local_vision = env::var("KERNEL_LOCAL_VISION")
            .map(|value| matches!(value.as_str(), "1" | "true" | "yes"))
            .unwrap_or(false);
        let pricing_ttl_seconds = env::var("KERNEL_PRICING_TTL_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(900u64)
            .clamp(60, 86_400);

        Ok(Self {
            bind,
            database_url,
            cloud: ModelConfig::from_env("CLOUD", "deepseek-chat"),
            local: ModelConfig::from_env("LOCAL", "local-model"),
            site: ProviderConfig::from_env("SITE"),
            crm: ProviderConfig::from_env("CRM"),
            site_token: env::var("KERNEL_SITE_TOKEN").unwrap_or_default(),
            crm_token: env::var("KERNEL_CRM_TOKEN").unwrap_or_default(),
            max_tool_steps,
            max_message_chars,
            max_output_tokens,
            max_attachments,
            max_attachment_bytes,
            cloud_vision,
            local_vision,
            harness: HarnessConfig::from_env(),
            pricing_ttl_seconds,
            allow_cloud_for_internal,
        })
    }

    pub fn provider(&self, channel: &str) -> Option<&ProviderConfig> {
        match channel {
            "site" => Some(&self.site),
            "crm" => Some(&self.crm),
            _ => None,
        }
    }

    pub fn inbound_token(&self, channel: &str) -> Option<&str> {
        let token = match channel {
            "site" => &self.site_token,
            "crm" => &self.crm_token,
            _ => return None,
        };
        (!token.is_empty()).then_some(token.as_str())
    }

    /// Site visitors are public. CRM data is internal and may include client
    /// details, so it must not silently fall back to a cloud model.
    pub fn default_data_class(&self, channel: &str) -> &'static str {
        match channel {
            "crm" => "internal",
            _ => "public",
        }
    }

    pub fn validate_data_class(&self, channel: &str, requested: &str) -> bool {
        match channel {
            "site" => requested == "public",
            "crm" => matches!(requested, "public" | "internal" | "secret"),
            _ => false,
        }
    }

    /// Vision support is a property of the endpoint, not of the kernel. A
    /// local llama.cpp server needs a multimodal projector to accept images.
    pub fn supports_vision(&self, route: crate::model::Route) -> bool {
        match route {
            crate::model::Route::Cloud => self.cloud_vision,
            crate::model::Route::Local => self.local_vision,
        }
    }
}
