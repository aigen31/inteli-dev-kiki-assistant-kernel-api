//! RFC 7807 problem responses and internal error mapping.

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::json;

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub detail: String,
}

impl ApiError {
    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_REQUEST, code: "bad_request", detail: detail.into() }
    }

    pub fn unauthorized(detail: impl Into<String>) -> Self {
        Self { status: StatusCode::UNAUTHORIZED, code: "unauthorized", detail: detail.into() }
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self { status: StatusCode::FORBIDDEN, code: "forbidden", detail: detail.into() }
    }

    pub fn not_found(detail: impl Into<String>) -> Self {
        Self { status: StatusCode::NOT_FOUND, code: "not_found", detail: detail.into() }
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self { status: StatusCode::CONFLICT, code: "conflict", detail: detail.into() }
    }

    /// The request is well-formed but this deployment cannot serve it, for
    /// example text-only model asked to read an image.
    pub fn unprocessable(detail: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: "unprocessable",
            detail: detail.into(),
        }
    }

    pub fn upstream(detail: impl Into<String>) -> Self {
        Self { status: StatusCode::BAD_GATEWAY, code: "upstream_error", detail: detail.into() }
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self { status: StatusCode::SERVICE_UNAVAILABLE, code: "unavailable", detail: detail.into() }
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self { status: StatusCode::INTERNAL_SERVER_ERROR, code: "internal_error", detail: detail.into() }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({
            "type": format!("https://inteli-dev.ru/errors/{}", self.code),
            "title": self.status.canonical_reason().unwrap_or("Error"),
            "status": self.status.as_u16(),
            "detail": self.detail,
            "code": self.code,
        });
        (self.status, [(header::CONTENT_TYPE, "application/problem+json")], body.to_string()).into_response()
    }
}

impl From<sqlx::Error> for ApiError {
    fn from(error: sqlx::Error) -> Self {
        tracing::error!(error = %error, "database error");
        ApiError::internal("storage error")
    }
}

impl From<reqwest::Error> for ApiError {
    fn from(error: reqwest::Error) -> Self {
        tracing::error!(error = %error, "upstream request error");
        ApiError::upstream("upstream request failed")
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
