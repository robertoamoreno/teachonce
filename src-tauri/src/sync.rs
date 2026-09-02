//! Handing a recording to a TeachOnce server.
//!
//! The whole session folder is zipped and posted, so the server holds exactly
//! what the app holds and can run the same pipeline over it. This is one of
//! the app's few outbound paths and it only runs when the user presses Submit.

use std::io::{Cursor, Write};
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use skillrec_core::config::ServerLink;

/// Zip a session folder with paths relative to its root.
pub fn zip_session(dir: &Path) -> Result<Vec<u8>> {
    let mut buffer = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .large_file(true);
        add_dir(&mut writer, dir, dir, options)?;
        writer.finish().context("finishing the archive")?;
    }
    Ok(buffer.into_inner())
}

fn add_dir(
    writer: &mut zip::ZipWriter<&mut Cursor<Vec<u8>>>,
    root: &Path,
    dir: &Path,
    options: zip::write::SimpleFileOptions,
) -> Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        // Atomic-write leftovers and Finder droppings are not part of a recording.
        if name.starts_with('.') || name.contains(".tmp.") {
            continue;
        }
        if path.is_dir() {
            add_dir(writer, root, &path, options)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .context("a file outside the recording folder")?
            .to_string_lossy()
            .replace('\\', "/");
        writer.start_file(relative, options).context("starting an archive entry")?;
        writer
            .write_all(&std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?)
            .context("writing an archive entry")?;
    }
    Ok(())
}

/// Upload a recording. Returns once the server has accepted it.
pub async fn submit(dir: &Path, link: &ServerLink) -> Result<()> {
    link.validate()?;
    let folder = dir.to_path_buf();
    let archive = tokio::task::spawn_blocking(move || zip_session(&folder))
        .await
        .context("the zip task failed")??;
    tracing::info!(bytes = archive.len(), server = %link.base(), "submitting a recording");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(900))
        .build()
        .context("building the HTTP client")?;
    let part = reqwest::multipart::Part::bytes(archive)
        .file_name("recording.zip")
        .mime_str("application/zip")
        .context("describing the upload")?;
    let form = reqwest::multipart::Form::new().part("file", part);
    let response = client
        .post(format!("{}/api/sessions/upload", link.base()))
        .bearer_auth(link.api_key.trim())
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("reaching {}", link.base()))?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    anyhow::ensure!(
        status.is_success(),
        "the server answered {status}: {}",
        body.chars().take(300).collect::<String>()
    );
    Ok(())
}

/// Prove the URL and the key together, with a call that changes nothing.
pub async fn test(link: &ServerLink) -> Result<String> {
    link.validate()?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building the HTTP client")?;
    let response = client
        .post(format!("{}/api/rpc/recorder_status", link.base()))
        .bearer_auth(link.api_key.trim())
        .header("content-type", "application/json")
        .body("{}")
        .send()
        .await
        .with_context(|| format!("Could not reach {}", link.base()))?;
    match response.status() {
        s if s.is_success() => Ok(format!("Connected to {}.", link.base())),
        reqwest::StatusCode::UNAUTHORIZED => anyhow::bail!("The server rejected the API key."),
        s => anyhow::bail!("The server answered {s}."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_folder_zips_with_relative_paths_and_no_temp_files() {
        let dir = std::env::temp_dir().join(format!("teachonce-zip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("frames")).unwrap();
        std::fs::write(dir.join("session.json"), "{}").unwrap();
        std::fs::write(dir.join("events.jsonl"), "{}\n").unwrap();
        std::fs::write(dir.join("frames/frame_000001.jpg"), "jpeg").unwrap();
        std::fs::write(dir.join("session.json.tmp.123.4"), "half-written").unwrap();
        std::fs::write(dir.join(".DS_Store"), "finder").unwrap();

        let archive = zip_session(&dir).unwrap();
        let mut zip = zip::ZipArchive::new(Cursor::new(archive)).unwrap();
        let mut names: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_string()).collect();
        names.sort();
        assert_eq!(names, vec!["events.jsonl", "frames/frame_000001.jpg", "session.json"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Against a real server: `TEACHONCE_TEST_SERVER=http://host:port
    /// TEACHONCE_TEST_KEY=tk_… cargo test -p teachonce -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a running server named by TEACHONCE_TEST_SERVER and TEACHONCE_TEST_KEY"]
    async fn a_real_server_accepts_what_the_app_sends() {
        let link = ServerLink {
            base_url: std::env::var("TEACHONCE_TEST_SERVER").expect("TEACHONCE_TEST_SERVER"),
            api_key: std::env::var("TEACHONCE_TEST_KEY").expect("TEACHONCE_TEST_KEY"),
        };
        assert!(test(&link).await.unwrap().starts_with("Connected"));

        let dir = std::env::temp_dir().join(format!("teachonce-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("frames")).unwrap();
        std::fs::write(
            dir.join("session.json"),
            r#"{"id":"20260901-000000-livetest","startedAt":1000,"stoppedAt":9000,"platform":"macos","appVersion":"test"}"#,
        )
        .unwrap();
        std::fs::write(dir.join("events.jsonl"), "").unwrap();
        submit(&dir, &link).await.unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn submitting_without_a_configured_server_is_refused_before_any_network() {
        let link = ServerLink::default();
        let err = submit(Path::new("/nonexistent"), &link).await.unwrap_err();
        assert!(format!("{err:#}").contains("no server URL"));
        assert!(test(&link).await.is_err());
    }
}
