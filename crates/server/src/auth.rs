//! The one shared API key, as a bearer token or, for the browser's event
//! stream (`EventSource` cannot set headers), a `key` query parameter.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::state::AppState;

pub async fn require_key(
    State(state): State<Arc<AppState>>,
    request: Request,
    next: Next,
) -> Response {
    let presented = bearer(&request).or_else(|| query_key(&request));
    let expected = state.config.read().await.api_key.clone();
    match presented {
        Some(key) if constant_time_eq(key.as_bytes(), expected.as_bytes()) => {
            next.run(request).await
        }
        _ => (StatusCode::UNAUTHORIZED, "missing or wrong API key").into_response(),
    }
}

fn bearer(request: &Request) -> Option<String> {
    let value = request.headers().get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix("Bearer ").or_else(|| value.strip_prefix("bearer "))?;
    Some(token.trim().to_string())
}

fn query_key(request: &Request) -> Option<String> {
    request
        .uri()
        .query()?
        .split('&')
        .find_map(|pair| pair.strip_prefix("key=").map(str::to_string))
}

/// Compare without leaking where the first difference is.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_compare_exactly() {
        assert!(constant_time_eq(b"tk_abc", b"tk_abc"));
        assert!(!constant_time_eq(b"tk_abc", b"tk_abd"));
        assert!(!constant_time_eq(b"tk_abc", b"tk_ab"));
        assert!(!constant_time_eq(b"", b"x"));
    }

    #[test]
    fn the_key_is_read_from_the_header_or_the_query() {
        let with_header = Request::builder()
            .uri("/api/rpc/x")
            .header("authorization", "Bearer tk_1")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(bearer(&with_header).as_deref(), Some("tk_1"));
        assert_eq!(query_key(&with_header), None);

        let with_query = Request::builder()
            .uri("/api/events?foo=1&key=tk_2")
            .body(axum::body::Body::empty())
            .unwrap();
        assert_eq!(bearer(&with_query), None);
        assert_eq!(query_key(&with_query).as_deref(), Some("tk_2"));
    }
}
