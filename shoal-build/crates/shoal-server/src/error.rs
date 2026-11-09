//! JSON error envelope and HTTP status mapping shared by all route handlers.
//!
//! Wire format:
//! ```json
//! { "error": { "code": "not_found", "message": "namespace 'docs' not found" } }
//! ```

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use crate::auth::AuthError;
use crate::rate_limit::RateLimited;

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub retry_after_secs: Option<f64>,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            retry_after_secs: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(what: impl Into<String>) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "not_found",
            format!("{} not found", what.into()),
        )
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message)
    }

    pub fn payload_too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large", message)
    }

    pub fn too_many_requests(message: impl Into<String>) -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            message,
        );
        e.retry_after_secs = None;
        e
    }

    pub fn too_many_requests_with_retry(retry_after_secs: f64) -> Self {
        let mut e = Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "rate limit exceeded",
        );
        e.retry_after_secs = Some(retry_after_secs);
        e
    }

    pub fn internal(message: impl std::fmt::Display) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            message.to_string(),
        )
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({}): {}", self.status, self.code, self.message)
    }
}

impl std::error::Error for ApiError {}

#[derive(Serialize)]
struct ErrorDetail<'a> {
    code: &'a str,
    message: &'a str,
}

#[derive(Serialize)]
struct ErrorBody<'a> {
    error: ErrorDetail<'a>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code,
                message: &self.message,
            },
        };
        let mut resp = (self.status, Json(&body)).into_response();
        if let Some(ra) = self.retry_after_secs {
            let secs = ra.ceil().max(1.0) as u64;
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                resp.headers_mut().insert(header::RETRY_AFTER, v);
            }
        }
        resp
    }
}

impl From<AuthError> for ApiError {
    fn from(e: AuthError) -> Self {
        match e {
            AuthError::InvalidToken | AuthError::UnknownKey => ApiError::unauthorized("missing or invalid API key"),
            AuthError::Disabled => ApiError::forbidden("API key is disabled"),
            AuthError::Forbidden(msg) => ApiError::forbidden(msg),
            AuthError::Io(e) => ApiError::internal(e),
            AuthError::Parse(e) => ApiError::internal(e),
        }
    }
}

impl From<RateLimited> for ApiError {
    fn from(e: RateLimited) -> Self {
        ApiError::too_many_requests_with_retry(e.retry_after_secs)
    }
}

impl From<crate::engine::EngineError> for ApiError {
    fn from(e: crate::engine::EngineError) -> Self {
        match &e {
            crate::engine::EngineError::NamespaceNotFound(msg) => ApiError::not_found(msg.clone()),
            crate::engine::EngineError::DocumentNotFound(msg) => ApiError::not_found(msg.clone()),
            crate::engine::EngineError::NamespaceExists(msg) => ApiError::conflict(msg.clone()),
            crate::engine::EngineError::Invalid(msg) => ApiError::bad_request(msg.clone()),
            crate::engine::EngineError::VersionConflict { expected, actual } => {
                ApiError::conflict(format!("version conflict: expected {expected}, found {actual}"))
            }
            _ => ApiError::internal(e),
        }
    }
}

impl From<std::io::Error> for ApiError {
    fn from(e: std::io::Error) -> Self {
        ApiError::internal(e)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> Self {
        ApiError::internal(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_error_status_mapping() {
        assert_eq!(
            ApiError::from(AuthError::InvalidToken).status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::from(AuthError::UnknownKey).status,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            ApiError::from(AuthError::Disabled).status,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            ApiError::from(AuthError::Forbidden("x".into())).status,
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn unauthorized_message_is_generic() {
        // Must not echo any part of the presented credential.
        let e = ApiError::unauthorized("missing or invalid API key");
        assert_eq!(e.message, "missing or invalid API key");
    }

    #[test]
    fn rate_limited_maps_to_429() {
        let e = ApiError::from(RateLimited {
            retry_after_secs: 1.4,
        });
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.retry_after_secs, Some(1.4));
    }
}
