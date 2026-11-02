//! HTTP surface of the assistant kernel.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

use crate::config::Config;
use crate::error::{ApiError, ApiResult};
use crate::provider::Actor;
use crate::service::{AssistantService, ChatInput};
use crate::store::StoredMessage;

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
}

#[derive(Debug, Serialize)]
pub struct ConversationResponse {
    pub conversation_id: String,
    pub messages: Vec<StoredMessage>,
}

#[derive(Debug, Deserialize)]
pub struct NoteRequest {
    pub content: String,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .route("/openapi.yaml", get(openapi))
        .route("/v1/chat", post(chat))
        .route(
            "/v1/conversations/:id",
            get(conversation).delete(delete_conversation),
        )
        .route("/v1/conversations/:id/notes", post(notes))
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

    let data_class = request
        .data_class
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| state.config.default_data_class(channel));
    if !state.config.validate_data_class(channel, data_class) {
        return Err(ApiError::forbidden(format!(
            "channel {channel} cannot use data_class {data_class}"
        )));
    }

    let correlation_id = correlation_id(&headers);
    tracing::info!(channel, actor = %request.actor.id, data_class, correlation_id, "chat request");

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
        })
        .await?;

    Ok(with_correlation(Json(output).into_response(), &correlation_id))
}

async fn conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<Json<ConversationResponse>> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    let messages = state.service.history(&id, channel).await?;
    Ok(Json(ConversationResponse { conversation_id: id, messages }))
}

async fn delete_conversation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    let channel = channel_from_headers(&headers)?;
    authenticate(&state.config, &headers, channel)?;
    state.service.delete(&id, channel).await?;
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

fn with_correlation(mut response: Response, correlation_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(correlation_id) {
        response.headers_mut().insert("x-correlation-id", value);
    }
    response
}
