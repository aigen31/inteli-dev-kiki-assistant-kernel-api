//! Chat orchestration: conversations, tool loop, model routing and persistence.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::SqlitePool;

use crate::config::Config;
use crate::error::{ApiError, ApiResult};
use crate::harness::HarnessClient;
use crate::model::{ChatMessage, ModelClient};
use crate::prompt;
use crate::provider::{Actor, ProviderClient};
use crate::store::{self, title_for, Conversation, ConversationSummary, StoredMessage, Study};
use crate::research;
use crate::study;

const HISTORY_LIMIT: i64 = 40;
const MAX_TITLE_CHARS: usize = 80;

const ALLOWED_IMAGE_MIMES: [&str; 4] = [
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/gif",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachmentInput {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
}

/// A document is reference-only: the kernel gets its manifest, never the text.
/// The model reads slices through the provider's `read_document` tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentInput {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub chars: Option<u64>,
    #[serde(default)]
    pub preview: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ChatInput {
    pub channel: String,
    pub message: String,
    pub conversation_id: Option<String>,
    pub actor: Actor,
    pub data_class: String,
    pub context: Value,
    pub correlation_id: String,
    pub attachments: Vec<AttachmentInput>,
    pub documents: Vec<DocumentInput>,
}

#[derive(Debug, Clone)]
pub struct RegenerateInput {
    pub channel: String,
    pub conversation_id: String,
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
    pub title: String,
    pub answer: String,
    pub message: Option<StoredMessage>,
    pub messages: Vec<StoredMessage>,
    pub source: String,
    pub route: String,
    pub model: String,
    pub usage: Usage,
    pub pending_action: Option<Value>,
    pub suggested_next: Vec<Value>,
    pub trace: Vec<Value>,
    pub correlation_id: String,
}

struct TurnRequest {
    channel: String,
    actor: Actor,
    data_class: String,
    context: Value,
    correlation_id: String,
    attachments: Vec<AttachmentInput>,
    documents: Vec<DocumentInput>,
}

pub struct AssistantService {
    config: Config,
    pool: SqlitePool,
    models: ModelClient,
    providers: HashMap<String, ProviderClient>,
    harness: HarnessClient,
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
            config.max_output_tokens,
        );
        let harness = HarnessClient::new(config.harness.clone());
        Self { config, pool, models, providers, harness }
    }

    pub async fn chat(&self, input: ChatInput) -> ApiResult<ChatOutput> {
        let provider = self.provider(&input.channel)?;

        if input.attachments.len() > self.config.max_attachments {
            return Err(ApiError::bad_request(format!(
                "a message may carry at most {} images",
                self.config.max_attachments
            )));
        }

        let title = title_for(&input.message);
        let conversation = store::get_or_create_conversation(
            &self.pool,
            input.conversation_id,
            &input.channel,
            &input.actor.id,
            &input.actor.role,
            &input.data_class,
            &title,
        )
        .await?;

        let attachments = attachments_json(&input.attachments, &input.documents);
        store::insert_message(
            &self.pool,
            &conversation.id,
            "user",
            &input.message,
            None,
            None,
            attachments.as_ref(),
        )
        .await?;

        self.run_turn(
            provider,
            &conversation,
            TurnRequest {
                channel: input.channel,
                actor: input.actor,
                data_class: input.data_class,
                context: input.context,
                correlation_id: input.correlation_id,
                attachments: input.attachments,
                documents: input.documents,
            },
        )
        .await
    }

    /// Re-run the last user turn without appending a duplicate question. The
    /// channel mirrors this by dropping its own trailing assistant message.
    pub async fn regenerate(&self, input: RegenerateInput) -> ApiResult<ChatOutput> {
        let provider = self.provider(&input.channel)?;
        let conversation = self
            .owned_conversation(&input.channel, &input.actor.id, &input.conversation_id)
            .await?;

        let last_user = store::latest_user_message(&self.pool, &conversation.id)
            .await?
            .ok_or_else(|| ApiError::bad_request("conversation has no user message to regenerate"))?;

        store::delete_messages_after(&self.pool, &conversation.id, &last_user.id).await?;

        let (attachments, documents) = attachments_from_message(&last_user);

        self.run_turn(
            provider,
            &conversation,
            TurnRequest {
                channel: input.channel,
                actor: input.actor,
                data_class: input.data_class,
                context: input.context,
                correlation_id: input.correlation_id,
                attachments,
                documents,
            },
        )
        .await
    }

    async fn run_turn(
        &self,
        provider: &ProviderClient,
        conversation: &Conversation,
        request: TurnRequest,
    ) -> ApiResult<ChatOutput> {
        let route = self.models.route_for(&request.data_class)?;

        if !request.attachments.is_empty() && !self.config.supports_vision(route) {
            return Err(ApiError::unprocessable(format!(
                "the {} model route is configured for text only; enable vision for this route to send images",
                route.label()
            )));
        }

        let images = self.load_images(provider, conversation, &request).await?;

        let profile = provider
            .profile(
                &request.channel,
                &request.actor,
                &conversation.id,
                &request.correlation_id,
            )
            .await?;

        let mut messages = vec![ChatMessage::system(prompt::system_for(&profile.system))];
        if !request.context.is_null()
            && request.context != json!({})
            && request.context != json!([])
        {
            messages.push(ChatMessage::system(format!(
                "Контекст канала: {}",
                request.context
            )));
        }

        if !request.documents.is_empty() {
            let mut manifest = String::from(
                "К сообщению приложены документы. Их текст не загружен в контекст целиком: \
                 используй инструмент read_document, чтобы прочитать нужный фрагмент \
                 (document_id, при необходимости query, offset и limit). Не выдумывай содержимое.\n",
            );

            for document in &request.documents {
                manifest.push_str(&format!(
                    "- id={} «{}» ({}), {} символов. Начало: {}\n",
                    document.id,
                    document.name.as_deref().unwrap_or("документ"),
                    document.mime.as_deref().unwrap_or("application/octet-stream"),
                    document.chars.unwrap_or(0),
                    truncate_text(document.preview.as_deref().unwrap_or(""), 240),
                ));
            }

            messages.push(ChatMessage::system(manifest));
        }

        let stored = trim_history(store::load_messages(&self.pool, &conversation.id, HISTORY_LIMIT).await?);
        let last_user_id = stored
            .iter()
            .rev()
            .find(|message| message.role == "user")
            .map(|message| message.id.clone());

        for message in stored {
            let is_current_turn = last_user_id.as_deref() == Some(message.id.as_str());
            messages.push(to_model_message(
                message,
                if is_current_turn { images.clone() } else { Vec::new() },
            ));
        }

        // Kernel-native tools are available to every channel and cost no
        // provider round trip.
        let mut tools = profile.tools.clone();
        tools.push(self.price_tool());

        let mut usage = Usage { prompt_tokens: 0, completion_tokens: 0 };
        let mut answer = String::new();
        let mut pending_action = None;
        let mut completed = false;
        let mut trace: Vec<Value> = Vec::new();
        let mut last_assistant_id: Option<String> = None;

        for step in 0..self.config.max_tool_steps {
            let model_answer = self.models.complete(route, &messages, &tools).await?;
            usage.prompt_tokens += model_answer.prompt_tokens;
            usage.completion_tokens += model_answer.completion_tokens;

            if model_answer.tool_calls.is_empty() {
                answer = model_answer.content.clone();
                let stored_message = store::insert_message(
                    &self.pool,
                    &conversation.id,
                    "assistant",
                    &answer,
                    None,
                    None,
                    None,
                )
                .await?;
                last_assistant_id = Some(stored_message.id);
                completed = true;
                break;
            }

            if !model_answer.content.trim().is_empty() {
                trace.push(json!({
                    "type": "note",
                    "step": step + 1,
                    "text": truncate_text(&model_answer.content, 280),
                }));
            }

            let raw_tool_calls = model_answer.raw_tool_calls.clone();
            let stored_assistant = store::insert_message(
                &self.pool,
                &conversation.id,
                "assistant",
                &model_answer.content,
                raw_tool_calls.as_ref(),
                None,
                None,
            )
            .await?;
            last_assistant_id = Some(stored_assistant.id);
            messages.push(ChatMessage::assistant(
                model_answer.content.clone(),
                raw_tool_calls.clone(),
            ));

            for call in &model_answer.tool_calls {
                let started = Instant::now();

                let outcome = if call.name == "get_price_list" {
                    match self.price_list(false, &request.correlation_id).await {
                        Ok(value) => crate::provider::ToolOutcome {
                            result: value,
                            pending_action: None,
                        },
                        Err(error) => crate::provider::ToolOutcome {
                            result: json!({"error": error.detail}),
                            pending_action: None,
                        },
                    }
                } else {
                    match provider
                        .call_tool(
                            &request.channel,
                            &request.actor,
                            &conversation.id,
                            &call.name,
                            call.arguments.clone(),
                            &request.correlation_id,
                        )
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            tracing::warn!(
                                channel = %request.channel,
                                tool = %call.name,
                                error = %error.detail,
                                "tool call failed"
                            );
                            crate::provider::ToolOutcome {
                                result: json!({"error": error.detail}),
                                pending_action: None,
                            }
                        }
                    }
                };

                let tool_content =
                    serde_json::to_string(&outcome.result).unwrap_or_else(|_| "{}".to_string());

                trace.push(json!({
                    "type": "tool",
                    "step": step + 1,
                    "name": call.name,
                    "status": if outcome.result.get("error").is_some() { "error" } else { "ok" },
                    "summary": truncate_text(&tool_content, 240),
                    "duration_ms": started.elapsed().as_millis() as u64,
                }));

                store::insert_message(
                    &self.pool,
                    &conversation.id,
                    "tool",
                    &tool_content,
                    None,
                    Some(&call.id),
                    None,
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
            // Исчерпали шаги, но уже что-то наработали: отдаём частичный ответ,
            // а не 502 — иначе канал показывает «ассистент недоступен» и теряет
            // весь прогресс (план, найденные данные, запущенные прогоны).
            if answer.trim().is_empty() {
                answer = "Не успел выполнить всё за один ход. Уточните, с чего продолжить.".to_string();
            } else {
                answer.push_str("\n\nНе успел выполнить всё за один ход — уточните, продолжить?");
            }
            completed = true;
        }

        if pending_action.is_some() && answer.trim().is_empty() {
            answer = "Действие подготовлено. Подтвердите его в интерфейсе.".to_string();
        }

        if let Some(message_id) = &last_assistant_id {
            store::set_message_trace(&self.pool, message_id, &Value::Array(trace.clone())).await?;
        }

        let messages = trim_history(store::load_messages(&self.pool, &conversation.id, HISTORY_LIMIT).await?);
        let last_assistant = messages
            .iter()
            .rev()
            .find(|message| message.role == "assistant")
            .cloned();

        let suggested_next = if profile.suggested_next.is_empty() {
            prompt::default_suggestions(&request.channel)
        } else {
            profile.suggested_next
        };

        let title = store::conversation_summary(&self.pool, &conversation.id)
            .await?
            .map(|summary| summary.title)
            .unwrap_or_else(|| conversation.title.clone());

        Ok(ChatOutput {
            conversation_id: conversation.id.clone(),
            title,
            answer,
            message: last_assistant,
            messages,
            source: format!("kernel:{}", route.label()),
            route: route.label().to_string(),
            model: self.models.model_name(route).to_string(),
            usage,
            pending_action,
            suggested_next,
            trace,
            correlation_id: request.correlation_id,
        })
    }

    /// Fetch images through the channel provider, which keeps ownership of
    /// storage and authorization. Bytes stay in memory only for this turn.
    async fn load_images(
        &self,
        provider: &ProviderClient,
        conversation: &Conversation,
        request: &TurnRequest,
    ) -> ApiResult<Vec<String>> {
        let mut images = Vec::with_capacity(request.attachments.len());

        for attachment in &request.attachments {
            let file = provider
                .fetch_file(
                    &request.channel,
                    &request.actor,
                    &conversation.id,
                    &attachment.id,
                    self.config.max_attachment_bytes,
                    &request.correlation_id,
                )
                .await
                .map_err(|error| match error.status.as_u16() {
                    404 => ApiError::bad_request(
                        "изображение не найдено или недоступно; прикрепите его заново",
                    ),
                    _ => error,
                })?;

            if !ALLOWED_IMAGE_MIMES.contains(&file.mime.as_str()) {
                return Err(ApiError::bad_request(format!(
                    "attachment {} is {}, only images are supported",
                    attachment.id, file.mime
                )));
            }

            let encoded = base64::engine::general_purpose::STANDARD.encode(&file.bytes);
            images.push(format!("data:{};base64,{}", file.mime, encoded));
        }

        Ok(images)
    }

    /// Kernel-native price tool, available to every channel.
    fn price_tool(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": "get_price_list",
                "description": "Актуальный прайс-лист inteli-dev: услуги, пакеты и цены. \
                                Вызывай перед тем, как назвать цену, оценить работу или \
                                собрать смету в задаче.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }
        })
    }

    /// One shared price snapshot for every assistant. The site owns the rows;
    /// the kernel caches them so a chat does not hit the site on every turn.
    pub async fn price_list(&self, force: bool, correlation_id: &str) -> ApiResult<Value> {
        const KEY: &str = "pricing:site";

        if !force {
            if let Some((raw, updated_at)) = store::cache_get(&self.pool, KEY).await? {
                if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                    if cache_age_seconds(&updated_at)
                        .map(|age| age < self.config.pricing_ttl_seconds as i64)
                        .unwrap_or(false)
                    {
                        return Ok(with_cache_meta(value, true, false, &updated_at));
                    }
                }
            }
        }

        let site = self
            .providers
            .get("site")
            .filter(|provider| provider.is_configured())
            .ok_or_else(|| ApiError::unavailable("site provider is not configured"))?;

        match site.prices("site", correlation_id).await {
            Ok(value) => {
                store::cache_put(&self.pool, KEY, &value.to_string()).await?;
                Ok(with_cache_meta(value, false, false, &chrono::Utc::now().to_rfc3339()))
            }
            Err(error) => {
                if let Some((raw, updated_at)) = store::cache_get(&self.pool, KEY).await? {
                    if let Ok(value) = serde_json::from_str::<Value>(&raw) {
                        tracing::warn!(
                            error = %error.detail,
                            "price list fetch failed, serving the cached snapshot"
                        );
                        return Ok(with_cache_meta(value, true, true, &updated_at));
                    }
                }
                Err(error)
            }
        }
    }

    /// Start a site study. The harness sidecar runs it in an isolated
    /// workspace; the kernel stores only the job handle and the result.
    #[allow(clippy::too_many_arguments)]
    pub async fn start_study(
        &self,
        channel: &str,
        actor_id: &str,
        url: &str,
        focus: Option<&str>,
        agents_md: Option<&str>,
        instructions: Option<&str>,
        correlation_id: &str,
    ) -> ApiResult<Study> {
        if !self.harness.is_configured() {
            return Err(ApiError::unavailable("site study runner is not configured"));
        }

        let url = study::validate_site_url(url)?;
        let prompt = study::task_prompt(&url, focus);
        let seed = study::workspace_seed(&url, agents_md, instructions);

        let run_id = self
            .harness
            .start_run(&prompt, &seed, None, correlation_id)
            .await?;

        let study = store::insert_study(&self.pool, channel, actor_id, &url, focus).await?;
        store::update_study(
            &self.pool,
            &study.id,
            store::StudyUpdate {
                runner_run_id: Some(&run_id),
                ..Default::default()
            },
        )
        .await?;

        self.study_owned(&study.id, channel, actor_id).await
    }

    /// Запускает свободную задачу в harness-сайдкаре. Ничего не сохраняет:
    /// результат забирается через `research_status` сразу после завершения.
    pub async fn start_research(
        &self,
        task: &str,
        url: Option<&str>,
        correlation_id: &str,
    ) -> ApiResult<Value> {
        if !self.harness.is_configured() {
            return Err(ApiError::unavailable("research runner is not configured"));
        }

        let prompt = research::task_prompt(task, url)?;
        let seed = json!({});
        let run_id = self
            .harness
            .start_run(&prompt, &seed, None, correlation_id)
            .await?;

        Ok(json!({ "run_id": run_id, "status": "running" }))
    }

    /// Опрашивает run и отдаёт компактный результат.
    pub async fn research_status(&self, run_id: &str, correlation_id: &str) -> ApiResult<Value> {
        let payload = self.harness.get_run(run_id, correlation_id).await?;

        Ok(json!({
            "run_id": run_id,
            "status": payload.get("status").and_then(Value::as_str).unwrap_or("running"),
            "answer": payload.get("final").and_then(Value::as_str),
            "error": payload.get("error").and_then(Value::as_str),
            "usage": payload.get("usage").cloned().unwrap_or(Value::Null),
            "files": payload.get("files").cloned().unwrap_or_else(|| json!({})),
        }))
    }

    pub async fn cancel_research(&self, run_id: &str, correlation_id: &str) -> ApiResult<Value> {
        self.harness.cancel_run(run_id, correlation_id).await?;

        Ok(json!({ "run_id": run_id, "status": "cancelled" }))
    }

    pub async fn refresh_study(
        &self,
        id: &str,
        channel: &str,
        actor_id: &str,
        correlation_id: &str,
    ) -> ApiResult<Study> {
        let study = self.study_owned(id, channel, actor_id).await?;

        if study.status != "running" {
            return Ok(study);
        }

        let Some(run_id) = study.runner_run_id.as_deref() else {
            return Ok(study);
        };

        let payload = match self.harness.get_run(run_id, correlation_id).await {
            Ok(payload) => payload,
            Err(error) => {
                tracing::warn!(run_id, error = %error.detail, "study poll failed");
                return Ok(study);
            }
        };

        let status = payload
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("running");
        let events = study::compact_events(payload.get("events").unwrap_or(&Value::Null), 120);
        let files = payload.get("files").cloned().unwrap_or_else(|| json!({}));
        let agents_md = files.get("AGENTS.md").and_then(Value::as_str);
        let notes = files.get("PRICE-NOTES.md").and_then(Value::as_str);
        let error = payload.get("error").and_then(Value::as_str);

        store::update_study(
            &self.pool,
            id,
            store::StudyUpdate {
                status: Some(status),
                events: Some(&events),
                agents_md,
                notes,
                error,
                ..Default::default()
            },
        )
        .await?;

        self.study_owned(id, channel, actor_id).await
    }

    pub async fn cancel_study(
        &self,
        id: &str,
        channel: &str,
        actor_id: &str,
        correlation_id: &str,
    ) -> ApiResult<Study> {
        let study = self.study_owned(id, channel, actor_id).await?;

        if let Some(run_id) = study.runner_run_id.as_deref() {
            let _ = self.harness.cancel_run(run_id, correlation_id).await;
        }

        store::update_study(
            &self.pool,
            id,
            store::StudyUpdate {
                status: Some("cancelled"),
                ..Default::default()
            },
        )
        .await?;

        self.study_owned(id, channel, actor_id).await
    }

    pub async fn studies(
        &self,
        channel: &str,
        actor_id: &str,
    ) -> ApiResult<Vec<Study>> {
        store::list_studies(&self.pool, channel, actor_id, 20).await
    }

    async fn study_owned(&self, id: &str, channel: &str, actor_id: &str) -> ApiResult<Study> {
        let study = store::find_study(&self.pool, id)
            .await?
            .ok_or_else(|| ApiError::not_found("study not found"))?;

        if study.channel != channel || study.actor_id != actor_id {
            return Err(ApiError::forbidden("study belongs to another actor"));
        }

        Ok(study)
    }

    pub async fn sessions(
        &self,
        channel: &str,
        actor_id: &str,
        search: Option<&str>,
        include_archived: bool,
        limit: i64,
        offset: i64,
    ) -> ApiResult<Vec<ConversationSummary>> {
        store::list_conversations(
            &self.pool,
            channel,
            actor_id,
            search,
            include_archived,
            limit.clamp(1, 100),
            offset.max(0),
        )
        .await
    }

    pub async fn summary(&self, id: &str) -> ApiResult<Option<ConversationSummary>> {
        store::conversation_summary(&self.pool, id).await
    }

    pub async fn update_session(
        &self,
        channel: &str,
        actor_id: &str,
        id: &str,
        title: Option<String>,
        pinned: Option<bool>,
        archived: Option<bool>,
    ) -> ApiResult<ConversationSummary> {
        self.owned_conversation(channel, actor_id, id).await?;

        let title = title.map(|value| {
            let trimmed = value.trim().to_string();
            trimmed.chars().take(MAX_TITLE_CHARS).collect::<String>()
        });
        if let Some(value) = &title {
            if value.is_empty() {
                return Err(ApiError::bad_request("title must not be empty"));
            }
        }

        store::update_conversation(&self.pool, id, title.as_deref(), pinned, archived).await?;
        store::conversation_summary(&self.pool, id)
            .await?
            .ok_or_else(|| ApiError::not_found("conversation not found"))
    }

    pub async fn history(
        &self,
        conversation_id: &str,
        channel: &str,
    ) -> ApiResult<Vec<StoredMessage>> {
        let conversation = store::find_conversation(&self.pool, conversation_id)
            .await?
            .ok_or_else(|| ApiError::not_found("conversation not found"))?;
        if conversation.channel != channel {
            return Err(ApiError::forbidden("conversation belongs to another channel"));
        }
        store::load_messages(&self.pool, conversation_id, 200).await
    }

    pub async fn delete(
        &self,
        conversation_id: &str,
        channel: &str,
        actor_id: &str,
    ) -> ApiResult<()> {
        self.owned_conversation(channel, actor_id, conversation_id).await?;
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
        store::insert_message(&self.pool, conversation_id, "assistant", content, None, None, None)
            .await
    }

    pub async fn ready(&self) -> bool {
        store::ready(&self.pool).await
    }

    fn provider(&self, channel: &str) -> ApiResult<&ProviderClient> {
        let provider = self
            .providers
            .get(channel)
            .ok_or_else(|| ApiError::bad_request("unknown channel"))?;
        if !provider.is_configured() {
            return Err(ApiError::unavailable(format!(
                "{channel} provider is not configured"
            )));
        }
        Ok(provider)
    }

    async fn owned_conversation(
        &self,
        channel: &str,
        actor_id: &str,
        id: &str,
    ) -> ApiResult<Conversation> {
        let conversation = store::find_conversation(&self.pool, id)
            .await?
            .ok_or_else(|| ApiError::not_found("conversation not found"))?;

        if conversation.channel != channel || conversation.actor_id != actor_id {
            return Err(ApiError::forbidden(
                "conversation belongs to another channel or actor",
            ));
        }

        Ok(conversation)
    }
}

/// Keep only complete tool-call groups and start at a user turn.
///
/// A rolling LIMIT can cut a conversation in the middle of an assistant
/// tool_call / tool_result pair. Replaying an orphan `tool` message makes the
/// model reject the whole request, which surfaced as a fast 502.
fn trim_history(messages: Vec<StoredMessage>) -> Vec<StoredMessage> {
    let start = messages
        .iter()
        .position(|message| message.role == "user")
        .unwrap_or(0);
    let slice = &messages[start..];
    let mut out = Vec::with_capacity(slice.len());
    let mut index = 0;

    while index < slice.len() {
        let message = &slice[index];

        if message.role == "tool" {
            index += 1;
            continue;
        }

        if message.role == "assistant" {
            let ids = tool_call_ids(&message.tool_calls);
            if !ids.is_empty() {
                let mut cursor = index + 1;
                let mut found = HashSet::new();

                while cursor < slice.len() && slice[cursor].role == "tool" {
                    if let Some(id) = &slice[cursor].tool_call_id {
                        found.insert(id.clone());
                    }
                    cursor += 1;
                }

                if ids.iter().all(|id| found.contains(id)) {
                    out.push(message.clone());
                    out.extend_from_slice(&slice[index + 1..cursor]);
                }

                index = cursor;
                continue;
            }
        }

        out.push(message.clone());
        index += 1;
    }

    out
}

fn tool_call_ids(value: &Option<Value>) -> Vec<String> {
    value
        .as_ref()
        .and_then(Value::as_array)
        .map(|calls| {
            calls
                .iter()
                .filter_map(|call| call.get("id").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn to_model_message(message: StoredMessage, images: Vec<String>) -> ChatMessage {
    match message.role.as_str() {
        "assistant" => ChatMessage::assistant(message.content, message.tool_calls),
        "tool" => ChatMessage::tool(message.tool_call_id.unwrap_or_default(), message.content),
        "user" => {
            let text = match attachment_count(&message.attachments) {
                0 => message.content,
                count if images.is_empty() => format!(
                    "{}\n\n[к сообщению приложено изображений: {count}; изображения доступны только в том сообщении, где их отправили]",
                    message.content
                ),
                _ => message.content,
            };
            ChatMessage::user_with_images(text, images)
        }
        _ => ChatMessage::system(message.content),
    }
}

fn attachment_count(attachments: &Option<Value>) -> usize {
    attachments
        .as_ref()
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("kind").and_then(Value::as_str) != Some("document"))
                .count()
        })
        .unwrap_or(0)
}

fn attachments_json(attachments: &[AttachmentInput], documents: &[DocumentInput]) -> Option<Value> {
    if attachments.is_empty() && documents.is_empty() {
        return None;
    }

    let mut items = Vec::with_capacity(attachments.len() + documents.len());

    for attachment in attachments {
        items.push(json!({
            "kind": "image",
            "id": attachment.id,
            "name": attachment.name,
            "mime": attachment.mime,
        }));
    }

    for document in documents {
        items.push(json!({
            "kind": "document",
            "id": document.id,
            "name": document.name,
            "mime": document.mime,
            "chars": document.chars,
            "preview": document.preview,
        }));
    }

    Some(Value::Array(items))
}

fn attachments_from_message(message: &StoredMessage) -> (Vec<AttachmentInput>, Vec<DocumentInput>) {
    let mut images = Vec::new();
    let mut documents = Vec::new();

    if let Some(items) = message.attachments.as_ref().and_then(Value::as_array) {
        for item in items {
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };

            if item.get("kind").and_then(Value::as_str) == Some("document") {
                documents.push(DocumentInput {
                    id: id.to_string(),
                    name: item.get("name").and_then(Value::as_str).map(str::to_string),
                    mime: item.get("mime").and_then(Value::as_str).map(str::to_string),
                    chars: item.get("chars").and_then(Value::as_u64),
                    preview: item.get("preview").and_then(Value::as_str).map(str::to_string),
                });
            } else {
                images.push(AttachmentInput {
                    id: id.to_string(),
                    name: item.get("name").and_then(Value::as_str).map(str::to_string),
                    mime: item.get("mime").and_then(Value::as_str).map(str::to_string),
                });
            }
        }
    }

    (images, documents)
}

fn truncate_text(value: &str, limit: usize) -> String {
    let clean = value.split_whitespace().collect::<Vec<_>>().join(" ");
    let total = clean.chars().count();
    let mut text: String = clean.chars().take(limit).collect();

    if total > limit {
        text.push('…');
    }

    text
}

fn cache_age_seconds(updated_at: &str) -> Option<i64> {
    let parsed = chrono::DateTime::parse_from_rfc3339(updated_at).ok()?;
    Some(
        chrono::Utc::now()
            .signed_duration_since(parsed.with_timezone(&chrono::Utc))
            .num_seconds(),
    )
}

fn with_cache_meta(mut value: Value, cached: bool, stale: bool, cached_at: &str) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.insert("cached".to_string(), Value::Bool(cached));
        object.insert("stale".to_string(), Value::Bool(stale));
        object.insert("cached_at".to_string(), Value::String(cached_at.to_string()));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles_collapse_whitespace_and_truncate() {
        assert_eq!(title_for("  Что   сегодня\nв работе?  "), "Что сегодня в работе?");
        assert_eq!(title_for("   "), "Новый диалог");
        assert_eq!(title_for(&"а".repeat(200)).chars().count(), MAX_TITLE_CHARS);
    }

    #[test]
    fn attachment_metadata_round_trips() {
        let items = vec![AttachmentInput {
            id: "8f".into(),
            name: Some("screen.png".into()),
            mime: Some("image/png".into()),
        }];
        let json = attachments_json(&items, &[]).expect("metadata");
        let message = StoredMessage {
            id: "m".into(),
            role: "user".into(),
            content: "смотри".into(),
            tool_calls: None,
            tool_call_id: None,
            attachments: Some(json),
            trace: None,
            created_at: "now".into(),
        };
        let (parsed, documents) = attachments_from_message(&message);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].id, "8f");
        assert!(documents.is_empty());
    }

    #[test]
    fn older_turns_keep_a_text_marker_instead_of_images() {
        let message = StoredMessage {
            id: "m".into(),
            role: "user".into(),
            content: "баг на главной".into(),
            tool_calls: None,
            tool_call_id: None,
            attachments: Some(json!([{"id": "a"}, {"id": "b"}])),
            trace: None,
            created_at: "now".into(),
        };
        let converted = to_model_message(message, Vec::new());
        match converted.content {
            Some(crate::model::MessageContent::Text(text)) => {
                assert!(text.contains("приложено изображений: 2"));
            }
            _ => panic!("expected text content"),
        }
    }
}
