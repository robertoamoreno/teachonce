//! Receiving a recording: a zip of the session folder, unpacked into place and
//! handed to the pipeline.

use std::io::Cursor;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;
use skillrec_core::paths::{is_valid_session_id, resolve_within};
use skillrec_core::session::SessionMeta;

use crate::jobs;
use crate::state::AppState;

pub async fn handle(State(state): State<Arc<AppState>>, mut multipart: Multipart) -> Response {
    match receive(&state, &mut multipart).await {
        Ok(id) => Json(json!({ "id": id })).into_response(),
        Err(err) => (StatusCode::BAD_REQUEST, format!("{err:#}")).into_response(),
    }
}

async fn receive(state: &Arc<AppState>, multipart: &mut Multipart) -> Result<String> {
    let mut archive = None;
    while let Some(field) = multipart.next_field().await.context("reading the upload")? {
        if field.name() == Some("file") {
            archive = Some(field.bytes().await.context("reading the archive")?);
        }
    }
    let archive = archive.context("the upload carried no `file` field")?;
    let sessions = skillrec_core::paths::sessions_root()?;
    let id = tokio::task::spawn_blocking(move || unpack(&archive, &sessions))
        .await
        .context("the unpack task failed")??;

    tracing::info!(%id, "recording received");
    state.set_job(&id, "received", "Recording received.").await;
    jobs::spawn_pipeline(Arc::clone(state), id.clone());
    Ok(id)
}

/// Unpack a session archive into `sessions_root/<id>`, replacing any earlier
/// copy of the same recording.
///
/// The folder is named after the id inside `session.json`, never after
/// anything the sender chose, and every entry path is checked against the
/// staging folder before a byte is written: an archive is data, and data does
/// not get to name files outside its own recording.
pub fn unpack(archive: &[u8], sessions_root: &Path) -> Result<String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive))
        .context("the upload is not a zip archive")?;
    let meta: SessionMeta = {
        let file = zip
            .by_name("session.json")
            .context("the archive has no session.json at its root")?;
        serde_json::from_reader(file).context("session.json is not a valid recording")?
    };
    anyhow::ensure!(is_valid_session_id(&meta.id), "invalid session id {:?}", meta.id);

    let staging = sessions_root.join(format!(".incoming-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&staging).context("creating the staging folder")?;
    let extracted = extract_all(&mut zip, &staging);
    if let Err(err) = extracted {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(err);
    }

    let target = sessions_root.join(&meta.id);
    if target.exists() {
        std::fs::remove_dir_all(&target).context("replacing the earlier copy")?;
    }
    std::fs::rename(&staging, &target).context("moving the recording into place")?;
    Ok(meta.id)
}

fn extract_all(zip: &mut zip::ZipArchive<Cursor<&[u8]>>, into: &Path) -> Result<()> {
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).context("reading an archive entry")?;
        let Some(relative) = entry.enclosed_name() else {
            anyhow::bail!("the archive contains an unsafe path {:?}", entry.name());
        };
        let target = resolve_within(into, &relative.to_string_lossy())?;
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&target)
            .with_context(|| format!("creating {}", target.display()))?;
        std::io::copy(&mut entry, &mut out).context("writing an archive entry")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn archive(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut buffer = Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            for (name, body) in entries {
                writer.start_file(*name, SimpleFileOptions::default()).unwrap();
                writer.write_all(body.as_bytes()).unwrap();
            }
            writer.finish().unwrap();
        }
        buffer.into_inner()
    }

    fn session_json(id: &str) -> String {
        format!(
            r#"{{"id":"{id}","startedAt":1000,"stoppedAt":2000,"platform":"macos","appVersion":"t"}}"#
        )
    }

    fn temp_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("teachonce-upload-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn a_recording_is_unpacked_under_its_own_id_and_replaced_on_resubmit() {
        let root = temp_root("ok");
        let first = archive(&[
            ("session.json", &session_json("20260901-120000-abcd1234")),
            ("events.jsonl", "{}\n"),
            ("frames/frame_000001.jpg", "jpeg"),
        ]);
        let id = unpack(&first, &root).unwrap();
        assert_eq!(id, "20260901-120000-abcd1234");
        assert!(root.join(&id).join("frames/frame_000001.jpg").exists());
        assert!(std::fs::read_dir(&root).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".incoming")));

        // Resubmitting the same id replaces the folder wholesale.
        let second = archive(&[("session.json", &session_json(&id)), ("analysis.json", "{}")]);
        unpack(&second, &root).unwrap();
        assert!(root.join(&id).join("analysis.json").exists());
        assert!(!root.join(&id).join("events.jsonl").exists());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn unsafe_archives_are_refused_and_leave_nothing_behind() {
        let root = temp_root("bad");
        let escaping = archive(&[
            ("session.json", &session_json("20260901-120000-abcd1234")),
            ("../outside.txt", "nope"),
        ]);
        assert!(unpack(&escaping, &root).is_err());
        assert!(!root.join("20260901-120000-abcd1234").exists());
        assert!(!root.parent().unwrap().join("outside.txt").exists());

        let bad_id = archive(&[("session.json", &session_json("../../etc"))]);
        assert!(unpack(&bad_id, &root).is_err());

        let no_meta = archive(&[("events.jsonl", "")]);
        assert!(unpack(&no_meta, &root).is_err());

        assert!(unpack(b"not a zip", &root).is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0, "no staging folders left");
        std::fs::remove_dir_all(&root).ok();
    }
}
