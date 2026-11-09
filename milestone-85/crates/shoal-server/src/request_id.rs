//! Request-ID propagation.
//!
//! Every request gets a unique ID that is (a) attached to the request
//! extensions so handlers, audit entries and error responses can reference
//! it, and (b) echoed back to the client in the `x-request-id` response
//! header. If the client supplies a well-formed `x-request-id` of its own we
//! honour it, which lets callers correlate retries end-to-end.

use axum::{
    extract::Request,
    http::HeaderValue,
    middleware::Next,
    response::Response,
};
use uuid::Uuid;

/// Header used for both inbound propagation and the response echo.
pub const HEADER: &str = "x-request-id";

/// Maximum length we accept from a client-supplied request ID.
const MAX_LEN: usize = 128;

/// Newtype stored in request extensions.
#[derive(Clone, Debug)]
pub struct RequestId(pub String);

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn acceptable(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= MAX_LEN
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Axum middleware: assign or propagate a request ID.
pub async fn middleware(mut req: Request, next: Next) -> Response {
    let id = req
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|v| acceptable(v))
        .map(str::to_owned)
        .unwrap_or_else(|| Uuid::new_v4().to_string());

    req.extensions_mut().insert(RequestId(id.clone()));

    let mut res = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        res.headers_mut().insert(HEADER, value);
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_reasonable_client_ids() {
        assert!(acceptable("abc-123_DEF.9"));
        assert!(acceptable(&"a".repeat(128)));
    }

    #[test]
    fn rejects_hostile_or_oversized_ids() {
        assert!(!acceptable(""));
        assert!(!acceptable(&"a".repeat(129)));
        assert!(!acceptable("has space"));
        assert!(!acceptable("newline\ninjection"));
        assert!(!acceptable("héader"));
    }
}
