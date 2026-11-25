//! HTTP surface of the assistant kernel.

use std::sync::Arc;

use axum::extract::{Path, Query, Request as AxumRequest, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::config::Config;
use crate::error::{ApiError, ApiResult};
use crate::provider::Actor;
use crate::service::{AssistantService, ChatInput, DocumentInput, RegenerateInput};
use crate::store::{ConversationSummary, StoredMessage, Study};

#[derive(Clone)]
pub struct AppState {
    pub service: Arc<AssistantService>,
    pub config: Config,
}

#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    pub channel: String,
    pub message: String,
    #[serde(default)]
    pub conversation_id: Option<String>,
    pub actor: Actor,
    #[serde(default)]
    pub data_class: Option<String>,
    #[serde(default)]
    pub context: Option<Value>,
    #[serde(default)]
    pub attachments: Vec<AskInput>,
    #[serde(default)]
    pub documents: Vec<DocumentRequest>,
}

/// A document reference. The text lives in the channel; the model reads slices
/// through the provider's `read_document` tool instead of inflating the prompt.
#[derive(Debug, Clone, Deserialize)]
pub struct DocumentRequest {
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

impl From<DocumentRequest> for DocumentInput {
    fn from(input: DocumentRequest) -> Self {
        Self {
            id: input.id,
            name: input.name,
            mime: input.mime,
            chars: input.chars,
            preview: input.preview,
        }
    }
}

/// An image reference, not the image itself. The kernel pulls bytes from the
/// channel provider so the domain keeps ownership of storage and access rules.
#[derive(Debug, Clone, Deserialize)]
pub struct AskInput {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
}

impl From<AskInput> for crate::service::AttachmentInput {
    fn from(input: AskInput) -> Self {
        Self { id: input.id, name: input.name, mime: input.mime }
    }
}

#[derive(Debug, Serialize)]
pub struct ConversationResponse {
    pub conversation_id: String,
    pub title: String,
    pub pinned: bool,
    pub archived: bool,
    pub messages: Vec<StoredMessage>,
}

#[derive(Debug, Serialize)]
pub struct SessionListResponse {
    pub sessions: Vec<ConversationSummary>,
}

#[derive(Debug, Deserialize)]
pub struct NoteRequest {
    pub content: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionQuery {
    pub actor_id: String,
    pub actor_role: Option<String>,
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub archived: Option<String>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub offset: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct ActorQuery {
    pub actor_id: String,
}

#[derive(Debug, Deserialize)]
pub struct SessionPatch {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub pinned: Option<bool>,
    #[serde(default)]
    pub archived: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct RegenerateRequest {
    pub actor: Actor,
    #[serde(default)]
    pub data_class: Option<String>,
    #[serde(default)]
    pub context: Option<Value>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/openapi.yaml", get(openapi))
        .route("/v1/chat", post(chat))
        .route("/v1/conversations", get(list_sessions))
        .route(
            "/v1/conversations/:id",
            get(conversation)
                .patch(update_session)
                .delete(delete_conversation),
        )
        .route("/v1/conversations/:id/notes", post(notes))
        .route("/v1/conversations/:id/regenerate", post(regenerate))
        .route("/v1/knowledge/pricing", get(pricing))
        .route("/v1/studies", get(list_studies).post(start_study))
        .route("/v1/studies/:id", get(study_status))
        .route("/v1/studies/:id/cancel", post(cancel_study))
        .route("/v1/harness/runs", post(start_research))
        .route("/v1/harness/runs/:id", get(research_status))
        .route("/v1/harness/runs/:id/cancel", post(cancel_research))
        .layer(middleware::from_fn(correlation_middleware))
        .layer(RequestBodyLimitLayer::new(2 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn live() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({"status": "live"})))
}

async fn ready(State(state): State<AppState>) -> impl IntoResponse {
    if state.service.ready().await {
        (StatusCode::OK, Json(json!({"status": "ready"})))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "database": false})),
        )
    }
}

async fn openapi() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/yaml; charset=utf-8")],
        include_str!("../openapi.yaml"),
    )
}

async fn chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ChatRequest>,
) -> ApiResult<Response> {
    let channel = request.channel.trim();
    authenticate(&state.config, &headers, channel)?;

    if request.message.trim().is_empty() {
        return Err(ApiError::bad_request("message must not be empty"));
    }
    if request.message.chars().count() > state.config.max_message_chars {
        return Err(ApiError::bad_request(format!(
            "message is longer than {} characters",
            state.config.max_message_chars
        )));
    }
    if request.actor.id.trim().is_empty() || request.actor.role.trim().is_empty() {
        return Err(ApiError::bad_request("actor.id and actor.role are required"));
    }

    let data_class = resolve_data_class(&state.config, channel, request.data_class.as_deref())?;
    let correlation_id = correlation_id(&headers);

    tracing::info!(
        channel,
        actor = %request.actor.id,
        data_class,
        attachments = request.attachments.len(),
        correlation_id,
        "chat request"
    );

    let output = state
        .service
        .chat(ChatInput {
            channel: channel.to_string(),
            message: request.message,
            conversation_id: request.conversation_id,
            actor: request.actor,
            data_class: data_class.to_string(),
            context: request.context.unwrap_or(Value::Null),
            correlation_id: correlation_id.clone(),
            attachments: request.attachments.into_iter().map(Into::into).collect(),
            documents: request.documents.into_iter().map(Into::into).collect(),
        })
        .await?;

    Ok(with_correlation(Json(output).into_response(), &correlation_id))
}

async fn regenerate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<RegenerateRequest>,
) -> ApiResult<Json<crate::service::ChatOutput>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;

    if request.actor.id.trim().is_empty() || request.actor.role.trim().is_empty() {
        return Err(ApiError::bad_request("actor.id and actor.role are required"));
    }

    let data_class = resolve_data_class(&state.config, channel, request.data_class.as_deref())?;
    let correlation_id = correlation_id(&headers);

    let output = state
        .service
        .regenerate(RegenerateInput {
            channel: channel.to_string(),
            conversation_id: id,
            actor: request.actor,
            data_class: data_class.to_string(),
            context: request.context.unwrap_or(Value::Null),
            correlation_id,
        })
        .await?;

    Ok(Json(output))
}

async fn list_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SessionQuery>,
) -> ApiResult<Json<SessionListResponse>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;

    let include_archived = query
        .archived
        .as_deref()
        .map(|value| matches!(value, "1" | "true" | "yes" | "all"))
        .unwrap_or(false);

    let sessions = state
        .service
        .sessions(
            channel,
            query.actor_id.trim(),
            query.q.as_deref(),
            include_archived,
            query.limit.unwrap_or(50),
            query.offset.unwrap_or(0),
        )
        .await?;

    Ok(Json(SessionListResponse { sessions }))
}

async fn update_session(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(actor): Query<ActorQuery>,
    Json(patch): Json<SessionPatch>,
) -> ApiResult<Json<ConversationSummary>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;

    if patch.title.is_none() && patch.pinned.is_none() && patch.archived.is_none() {
        return Err(ApiError::bad_request(
            "provide at least one of title, pinned, archived",
        ));
    }

    let summary = state
        .service
        .update_session(
            channel,
            actor.actor_id.trim(),
            &id,
            patch.title,
            patch.pinned,
            patch.archived,
        )
        .await?;

    Ok(Json(summary))
}

async fn conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<ConversationResponse>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    let messages = state.service.history(&id, channel).await?;
    let summary = state.service.summary(&id).await?;

    Ok(Json(ConversationResponse {
        conversation_id: id,
        title: summary.as_ref().map(|item| item.title.clone()).unwrap_or_default(),
        pinned: summary.as_ref().map(|item| item.pinned).unwrap_or(false),
        archived: summary.as_ref().map(|item| item.archived).unwrap_or(false),
        messages,
    }))
}

async fn delete_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(actor): Query<ActorQuery>,
) -> ApiResult<StatusCode> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    state
        .service
        .delete(&id, channel, actor.actor_id.trim())
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn notes(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(request): Json<NoteRequest>,
) -> ApiResult<Json<StoredMessage>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    if request.content.trim().is_empty() {
        return Err(ApiError::bad_request("note content must not be empty"));
    }
    let message = state
        .service
        .append_note(&id, channel, request.content.trim())
        .await?;
    Ok(Json(message))
}

#[derive(Debug, Deserialize)]
struct ResearchRequest {
    task: String,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StudyRequest {
    pub actor: Actor,
    pub url: String,
    #[serde(default)]
    pub focus: Option<String>,
    #[serde(default)]
    pub agents_md: Option<String>,
    #[serde(default)]
    pub instructions: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StudyQuery {
    pub actor_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PricingQuery {
    #[serde(default)]
    pub force: Option<String>,
}

async fn pricing(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<PricingQuery>,
) -> ApiResult<Json<Value>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;

    let force = query
        .force
        .as_deref()
        .map(|value| matches!(value, "1" | "true" | "yes"))
        .unwrap_or(false);
    let correlation_id = correlation_id(&headers);

    Ok(Json(
        state.service.price_list(force, &correlation_id).await?,
    ))
}

async fn start_study(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<StudyRequest>,
) -> ApiResult<Json<Study>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;

    if request.actor.id.trim().is_empty() {
        return Err(ApiError::bad_request("actor.id is required"));
    }

    let correlation_id = correlation_id(&headers);

    let study = state
        .service
        .start_study(
            channel,
            request.actor.id.trim(),
            &request.url,
            request.focus.as_deref(),
            request.agents_md.as_deref(),
            request.instructions.as_deref(),
            &correlation_id,
        )
        .await?;

    Ok(Json(study))
}

async fn study_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(actor): Query<StudyQuery>,
) -> ApiResult<Json<Study>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    let correlation_id = correlation_id(&headers);

    Ok(Json(
        state
            .service
            .refresh_study(&id, channel, actor.actor_id.trim(), &correlation_id)
            .await?,
    ))
}

async fn cancel_study(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(actor): Query<StudyQuery>,
) -> ApiResult<Json<Study>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    let correlation_id = correlation_id(&headers);

    Ok(Json(
        state
            .service
            .cancel_study(&id, channel, actor.actor_id.trim(), &correlation_id)
            .await?,
    ))
}

async fn start_research(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<ResearchRequest>,
) -> ApiResult<Json<Value>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    let correlation_id = correlation_id(&headers);

    let payload = state
        .service
        .start_research(&request.task, request.url.as_deref(), &correlation_id)
        .await?;

    Ok(Json(payload))
}

async fn research_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    let correlation_id = correlation_id(&headers);

    Ok(Json(state.service.research_status(&id, &correlation_id).await?))
}

async fn cancel_research(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    let correlation_id = correlation_id(&headers);

    Ok(Json(state.service.cancel_research(&id, &correlation_id).await?))
}

async fn list_studies(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(actor): Query<StudyQuery>,
) -> ApiResult<Json<Value>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;

    let studies = state
        .service
        .studies(channel, actor.actor_id.trim())
        .await?;

    Ok(Json(json!({ "studies": studies })))
}

fn resolve_data_class<'a>(
    config: &Config,
    channel: &str,
    requested: Option<&'a str>,
) -> ApiResult<&'a str> {
    let value = requested
        .map(str::trim)
        .filter(|value| !value.is_empty());

    match value {
        Some(value) => {
            if !config.validate_data_class(channel, value) {
                return Err(ApiError::forbidden(format!(
                    "channel {channel} cannot use data_class {value}"
                )));
            }
            Ok(value)
        }
        None => Ok(config.default_data_class(channel)),
    }
}

fn channel_from_headers(headers: &HeaderMap) -> ApiResult<&str> {
    headers
        .get("x-kernel-channel")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| *value == "site" || *value == "crm")
        .ok_or_else(|| ApiError::bad_request("x-kernel-channel must be site or crm"))
}

fn authenticate(config: &Config, headers: &HeaderMap, channel: &str) -> ApiResult<()> {
    let expected = config
        .inbound_token(channel)
        .ok_or_else(|| ApiError::unauthorized("channel token is not configured"))?;
    let provided = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .ok_or_else(|| ApiError::unauthorized("missing bearer token"))?;

    if !constant_time_eq(provided.as_bytes(), expected.as_bytes()) {
        return Err(ApiError::unauthorized("invalid bearer token"));
    }
    Ok(())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right) {
        difference |= a ^ b;
    }
    difference == 0
}

fn correlation_id(headers: &HeaderMap) -> String {
    headers
        .get("x-correlation-id")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 64)
        .map(str::to_string)
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

/// One correlation id per request: reuse the caller's, generate otherwise, and
/// echo it on the response so a channel can log the same value.
async fn correlation_middleware(mut request: AxumRequest, next: Next) -> Response {
    let id = correlation_id(request.headers());
    if let Ok(value) = HeaderValue::from_str(&id) {
        request.headers_mut().insert("x-correlation-id", value);
    }

    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert("x-correlation-id", value);
    }
    response
}

fn with_correlation(mut response: Response, correlation_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(correlation_id) {
        response.headers_mut().insert("x-correlation-id", value);
    }
    response
}
