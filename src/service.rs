//! Chat orchestration: conversation, tool loop, model routing and persistence.

use std::collections::HashMap;
use std::sync::Arc;

use serde::Serialize;
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::error::{ApiError, ApiResult};
use crate::model::{ChatMessage, ModelClient};
use crate::prompt;
use crate::provider::{Actor, ProviderClient};
use crate::store::{self, StoredMessage};

#[derive(Debug, Clone)]
pub struct ChatInput {
    pub channel: String,
    pub message: String,
    pub conversation_id: Option<String>,
    pub actor: Actor,
    pub data_class: String,
    pub context: Value,
    pub correlation_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatOutput {
    pub conversation_id: String,
    pub answer: String,
    pub message: Option<StoredMessage>,
    pub messages: Vec<StoredMessage>,
    pub source: String,
    pub route: String,
    pub model: String,
    pub usage: Usage,
    pub pending_action: Option<Value>,
    pub suggested_next: Vec<Value>,
    pub correlation_id: String,
}

pub struct AssistantService {
    #[allow(dead_code)]
    config: Config,
    pool: SqlitePool,
    models: ModelClient,
    providers: HashMap<String, ProviderClient>,
}

impl AssistantService {
    pub fn new(config: Config, pool: SqlitePool) -> Self {
        let mut providers = HashMap::new();
        providers.insert("site".to_string(), ProviderClient::new(config.site.clone()));
        providers.insert("crm".to_string(), ProviderClient::new(config.crm.clone()));
        let models = ModelClient::new(
            config.cloud.clone(),
            config.local.clone(),
            config.allow_cloud_for_internal,
        );
        Self { config, pool, models, providers }
    }

    pub async fn chat(&self, input: ChatInput) -> ApiResult<ChatOutput> {
        let provider = self
            .providers
            .get(&input.channel)
            .ok_or_else(|| ApiError::bad_request("unknown channel"))?;
        if !provider.is_configured() {
            return Err(ApiError::unavailable(format!(
                "{} provider is not configured",
                input.channel
            )));
        }

        let conversation = store::get_or_create_conversation(
            &self.pool,
            input.conversation_id,
            &input.channel,
            &input.actor.id,
            &input.actor.role,
            &input.data_class,
        )
        .await?;

        store::insert_message(
            &self.pool,
            &conversation.id,
            "user",
            &input.message,
            None,
            None,
        )
        .await?;

        let profile = provider.profile(&input.channel, &input.actor).await?;
        let mut messages = vec![ChatMessage::system(prompt::system_for(&profile.system))];
        if !input.context.is_null()
            && input.context != serde_json::json!({})
            && input.context != serde_json::json!([])
        {
            messages.push(ChatMessage::system(format!(
                "Контекст канала: {}",
                input.context
            )));
        }
        for message in store::load_messages(&self.pool, &conversation.id, 40).await? {
            messages.push(to_model_message(message));
        }

        let route = self.models.route_for(&input.data_class)?;
        let mut usage = Usage { prompt_tokens: 0, completion_tokens: 0 };
        let mut answer = String::new();
        let mut pending_action = None;
        let mut completed = false;

        for _ in 0..self.config.max_tool_steps {
            let model_answer = self
                .models
                .complete(route, &messages, &profile.tools)
                .await?;
            usage.prompt_tokens += model_answer.prompt_tokens;
            usage.completion_tokens += model_answer.completion_tokens;

            if model_answer.tool_calls.is_empty() {
                answer = model_answer.content.clone();
                store::insert_message(
                    &self.pool,
                    &conversation.id,
                    "assistant",
                    &answer,
                    None,
                    None,
                )
                .await?;
                completed = true;
                break;
            }

            let raw_tool_calls = model_answer.raw_tool_calls.clone();
            store::insert_message(
                &self.pool,
                &conversation.id,
                "assistant",
                &model_answer.content,
                raw_tool_calls.as_ref(),
                None,
            )
            .await?;
            messages.push(ChatMessage::assistant(
                model_answer.content.clone(),
                raw_tool_calls.clone(),
            ));

            for call in &model_answer.tool_calls {
                let outcome = match provider
                    .call_tool(
                        &input.channel,
                        &input.actor,
                        &conversation.id,
                        &call.name,
                        call.arguments.clone(),
                    )
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        tracing::warn!(
                            channel = %input.channel,
                            tool = %call.name,
                            error = %error.detail,
                            "tool call failed"
                        );
                        crate::provider::ToolOutcome {
                            result: json!({"error": error.detail}),
                            pending_action: None,
                        }
                    }
                };

                let tool_content = serde_json::to_string(&outcome.result)
                    .unwrap_or_else(|_| "{}".to_string());
                store::insert_message(
                    &self.pool,
                    &conversation.id,
                    "tool",
                    &tool_content,
                    None,
                    Some(&call.id),
                )
                .await?;
                messages.push(ChatMessage::tool(call.id.clone(), tool_content));

                if let Some(action) = outcome.pending_action {
                    pending_action = Some(action);
                    answer = model_answer.content.clone();
                    break;
                }
            }

            if pending_action.is_some() {
                completed = true;
                break;
            }
            answer = model_answer.content;
        }

        if !completed {
            return Err(ApiError::upstream("tool loop exceeded the configured limit"));
        }

        if pending_action.is_some() && answer.trim().is_empty() {
            answer = "Действие подготовлено. Подтвердите его в интерфейсе.".to_string();
        }

        let messages = store::load_messages(&self.pool, &conversation.id, 40).await?;
        let last_assistant = messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .cloned();

        let suggested_next = if profile.suggested_next.is_empty() {
            prompt::default_suggestions(&input.channel)
        } else {
            profile.suggested_next
        };

        Ok(ChatOutput {
            conversation_id: conversation.id,
            answer,
            message: last_assistant,
            messages,
            source: format!("kernel:{}", route.label()),
            route: route.label().to_string(),
            model: self.models.model_name(route).to_string(),
            usage,
            pending_action,
            suggested_next,
            correlation_id: input.correlation_id,
        })
    }

    pub async fn history(&self, conversation_id: &str, channel: &str) -> ApiResult<Vec<StoredMessage>> {
        let conversation = store::find_conversation(&self.pool, conversation_id)
            .await?
            .ok_or_else(|| ApiError::not_found("conversation not found"))?;
        if conversation.channel != channel {
            return Err(ApiError::forbidden("conversation belongs to another channel"));
        }
        store::load_messages(&self.pool, conversation_id, 200).await
    }

    pub async fn delete(&self, conversation_id: &str, channel: &str) -> ApiResult<()> {
        let conversation = store::find_conversation(&self.pool, conversation_id)
            .await?
            .ok_or_else(|| ApiError::not_found("conversation not found"))?;
        if conversation.channel != channel {
            return Err(ApiError::forbidden("conversation belongs to another channel"));
        }
        store::delete_conversation(&self.pool, conversation_id).await
    }

    pub async fn append_note(
        &self,
        conversation_id: &str,
        channel: &str,
        content: &str,
    ) -> ApiResult<StoredMessage> {
        let conversation = store::find_conversation(&self.pool, conversation_id)
            .await?
            .ok_or_else(|| ApiError::not_found("conversation not found"))?;
        if conversation.channel != channel {
            return Err(ApiError::forbidden("conversation belongs to another channel"));
        }
        store::insert_message(&self.pool, conversation_id, "assistant", content, None, None).await
    }

    pub async fn ready(&self) -> bool {
        store::ready(&self.pool).await
    }
}

fn to_model_message(message: StoredMessage) -> ChatMessage {
    match message.role.as_str() {
        "assistant" => ChatMessage {
            role: "assistant".into(),
            content: Some(message.content),
            tool_calls: message.tool_calls,
            tool_call_id: None,
        },
        "tool" => ChatMessage {
            role: "tool".into(),
            content: Some(message.content),
            tool_calls: None,
            tool_call_id: message.tool_call_id,
        },
        _ => ChatMessage {
            role: message.role,
            content: Some(message.content),
            tool_calls: None,
            tool_call_id: None,
        },
    }
}
