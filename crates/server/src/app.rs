//! The router: a health check, the key-protected API, and the embedded UI.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{DefaultBodyLimit, State};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{middleware, Json, Router};
use futures_util::{Stream, StreamExt};
use serde_json::{json, Value};
use tokio_stream::wrappers::BroadcastStream;

use crate::state::AppState;
use crate::{assets, auth, download, rpc, upload};

/// A recording with 600 stills and a long narration is a few hundred MB.
const UPLOAD_LIMIT: usize = 2 * 1024 * 1024 * 1024;

pub fn router(state: Arc<AppState>) -> Router {
    let protected = Router::new()
        .route("/api/rpc/{command}", post(rpc::handle))
        .route(
            "/api/sessions/upload",
            post(upload::handle).layer(DefaultBodyLimit::max(UPLOAD_LIMIT)),
        )
        .route("/api/events", get(events))
        .route("/api/sessions/{id}/skill.zip", get(download::skill))
        .route_layer(middleware::from_fn_with_state(Arc::clone(&state), auth::require_key));

    Router::new()
        .route("/api/health", get(health))
        .merge(protected)
        .fallback(assets::serve)
        .with_state(state)
}

async fn health() -> Json<Value> {
    Json(json!({ "name": "TeachOnce Server", "version": env!("CARGO_PKG_VERSION") }))
}

/// Server-sent events: every broadcast event as one JSON line, the same names
/// and payloads the desktop app receives from Tauri.
async fn events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|item| async move {
        let event = item.ok()?;
        let data = serde_json::to_string(&event).ok()?;
        Some(Ok(SseEvent::default().data(data)))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn state() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!("teachonce-router-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sessions")).unwrap();
        unsafe { std::env::set_var("SKILLREC_DATA_DIR", &dir) };
        let config = crate::config::ServerConfig { api_key: "tk_test".into(), ..Default::default() };
        Arc::new(AppState::new(dir.clone(), dir.join("server.json"), config))
    }

    async fn text(response: axum::response::Response) -> String {
        String::from_utf8(response.into_body().collect().await.unwrap().to_bytes().to_vec()).unwrap()
    }

    #[tokio::test]
    async fn health_is_open_and_everything_else_needs_the_key() {
        let app = router(state());

        let health = app
            .clone()
            .oneshot(Request::get("/api/health").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(health.status(), StatusCode::OK);
        assert!(text(health).await.contains("TeachOnce Server"));

        let no_key = app
            .clone()
            .oneshot(Request::post("/api/rpc/recorder_status").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(no_key.status(), StatusCode::UNAUTHORIZED);

        let wrong_key = app
            .clone()
            .oneshot(
                Request::post("/api/rpc/recorder_status")
                    .header("authorization", "Bearer tk_wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_key.status(), StatusCode::UNAUTHORIZED);

        let ok = app
            .clone()
            .oneshot(
                Request::post("/api/rpc/recorder_status")
                    .header("authorization", "Bearer tk_test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        let body: Value = serde_json::from_str(&text(ok).await).unwrap();
        assert_eq!(body["recording"], false);

        let desktop_only = app
            .oneshot(
                Request::post("/api/rpc/start_recording")
                    .header("authorization", "Bearer tk_test")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(desktop_only.status(), StatusCode::BAD_REQUEST);
        assert!(text(desktop_only).await.contains("desktop app"));
    }

    #[tokio::test]
    async fn a_built_skill_downloads_as_a_zip_and_a_missing_one_is_a_404() {
        let state = state();
        let sessions = state.data_dir.join("sessions");
        std::fs::create_dir_all(sessions.join("20260901-000000-rt000001")).unwrap();
        std::fs::create_dir_all(sessions.join("20260901-000000-rt000002")).unwrap();
        let skill = skillrec_core::skill::BuiltSkill {
            name: "file-expenses".into(),
            body: "Do it.".into(),
            ..Default::default()
        };
        skillrec_core::session::write_json(&sessions.join("20260901-000000-rt000001/skill.json"), &skill).unwrap();
        let app = router(state);

        let get = |path: &str, key: Option<&str>| {
            let mut req = Request::builder().method("GET").uri(path);
            if let Some(key) = key {
                req = req.header("authorization", format!("Bearer {key}"));
            }
            req.body(axum::body::Body::empty()).unwrap()
        };
        let no_key = app.clone().oneshot(get("/api/sessions/20260901-000000-rt000001/skill.zip", None)).await.unwrap();
        assert_eq!(no_key.status(), StatusCode::UNAUTHORIZED);

        let ok = app.clone().oneshot(get("/api/sessions/20260901-000000-rt000001/skill.zip", Some("tk_test"))).await.unwrap();
        assert_eq!(ok.status(), StatusCode::OK);
        assert_eq!(ok.headers()["content-type"], "application/zip");
        assert_eq!(ok.headers()["content-disposition"], "attachment; filename=\"file-expenses.zip\"");
        let bytes = ok.into_body().collect().await.unwrap().to_bytes();
        assert!(bytes.starts_with(b"PK"), "a zip archive");

        let none = app.oneshot(get("/api/sessions/20260901-000000-rt000002/skill.zip", Some("tk_test"))).await.unwrap();
        assert_eq!(none.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn an_empty_library_lists_as_empty_and_the_ui_shell_is_served() {
        let app = router(state());
        let list = app
            .clone()
            .oneshot(
                Request::post("/api/rpc/list_sessions")
                    .header("authorization", "Bearer tk_test")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(list.status(), StatusCode::OK);
        assert_eq!(text(list).await, "[]");

        let shell = app.oneshot(Request::get("/library/anything").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(shell.status(), StatusCode::OK);
        assert!(text(shell).await.contains("<div id=\"root\">"), "client routes get index.html");
    }
}
