//! The review UI, embedded from `ui/dist` so the server is one binary.
//!
//! The same React build runs in the desktop app; in a browser its transport
//! layer switches from Tauri commands to this server's RPC route, so there is
//! one UI to maintain. Build it with `npm run build` before building the
//! server; in a debug build the files are read from disk on each request.

use axum::http::{header, StatusCode, Uri};
use axum::response::{Html, IntoResponse, Response};

#[derive(rust_embed::RustEmbed)]
#[folder = "../../ui/dist"]
struct Assets;

pub async fn serve(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(file) = Assets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return ([(header::CONTENT_TYPE, mime.as_ref().to_string())], file.data.into_owned())
            .into_response();
    }
    // Anything else is a client-side route: hand back the app shell.
    match Assets::get("index.html") {
        Some(index) => Html(index.data.into_owned()).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            "the web UI is not built; run `npm run build` and rebuild the server",
        )
            .into_response(),
    }
}
