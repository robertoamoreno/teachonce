//! Fetching the Whisper weights, once.
//!
//! Nothing is bundled with the app: the models range from 75 MB to 1.6 GB and
//! most users will only ever want one. The download is explicit, resumable in
//! the sense that a failed attempt leaves no half-file behind, and reports
//! progress because a 466 MB fetch on a slow connection is otherwise a very
//! quiet minute.

use std::path::PathBuf;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use skillrec_core::config::WhisperModel;

/// Download progress, streamed to the UI.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    /// 0.0 to 1.0, or 0.0 when the server sent no content length.
    pub fraction: f64,
}

/// Where a model's weights live once fetched.
pub fn model_path(model: WhisperModel) -> Result<PathBuf> {
    Ok(skillrec_core::paths::models_root()?.join(model.file_name()))
}

/// Is this model already on disk?
pub fn is_model_cached(model: WhisperModel) -> bool {
    model_path(model)
        .ok()
        .and_then(|path| std::fs::metadata(path).ok())
        // A zero-length file is a failed download, not a cached model.
        .is_some_and(|meta| meta.is_file() && meta.len() > 0)
}

/// Ensure the weights are present, downloading them if they are not.
pub async fn ensure_model(
    model: WhisperModel,
    on_progress: &(dyn Fn(DownloadProgress) + Send + Sync),
) -> Result<PathBuf> {
    let path = model_path(model)?;
    if is_model_cached(model) {
        return Ok(path);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let url = model.download_url();
    tracing::info!(%url, "downloading Whisper weights");
    let response = reqwest::get(&url)
        .await
        .with_context(|| format!("requesting {url}"))?
        .error_for_status()
        .with_context(|| format!("downloading {url}"))?;

    let total = response.content_length().unwrap_or(0);
    // Download to a temp file and rename on success, so an interrupted fetch
    // never leaves a truncated model that whisper.cpp would fail to load with a
    // deeply unhelpful error.
    let temp = path.with_extension(format!("part.{}", std::process::id()));
    let mut file = tokio::fs::File::create(&temp)
        .await
        .with_context(|| format!("creating {}", temp.display()))?;

    let mut downloaded = 0u64;
    let mut last_reported = 0u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading the download stream")?;
        downloaded += chunk.len() as u64;
        tokio::io::AsyncWriteExt::write_all(&mut file, &chunk)
            .await
            .context("writing the model file")?;

        // Report about every megabyte rather than every chunk; a 466 MB download
        // is tens of thousands of chunks and the UI does not need them all.
        if downloaded - last_reported >= 1_000_000 || downloaded == total {
            last_reported = downloaded;
            on_progress(DownloadProgress {
                downloaded_bytes: downloaded,
                total_bytes: total,
                fraction: if total > 0 { downloaded as f64 / total as f64 } else { 0.0 },
            });
        }
    }
    tokio::io::AsyncWriteExt::flush(&mut file).await.ok();
    drop(file);

    if downloaded == 0 {
        let _ = std::fs::remove_file(&temp);
        anyhow::bail!("the download returned no data");
    }
    std::fs::rename(&temp, &path)
        .with_context(|| format!("moving the download into {}", path.display()))?;
    tracing::info!(mb = downloaded / 1_000_000, "Whisper weights ready");
    Ok(path)
}

/// Delete a cached model, for freeing disk space from the UI.
pub fn remove_model(model: WhisperModel) -> Result<()> {
    let path = model_path(model)?;
    if path.exists() {
        std::fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_checkpoint_has_a_distinct_file_and_url() {
        let models = [
            WhisperModel::Tiny,
            WhisperModel::Base,
            WhisperModel::Small,
            WhisperModel::Medium,
            WhisperModel::LargeV3Turbo,
        ];
        let mut names: Vec<&str> = models.iter().map(|m| m.file_name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), models.len(), "checkpoints must not share a filename");

        for model in models {
            assert!(model.download_url().ends_with(model.file_name()));
            assert!(model.download_url().starts_with("https://"));
            assert!(model.approx_mb() > 0);
        }
    }

    #[test]
    fn a_missing_or_empty_model_does_not_count_as_cached() {
        let dir = std::env::temp_dir().join(format!("skillrec-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        unsafe { std::env::set_var("SKILLREC_MODELS_DIR", &dir) };

        assert!(!is_model_cached(WhisperModel::Tiny));

        // A zero-byte file is what an interrupted download leaves behind.
        std::fs::write(dir.join(WhisperModel::Tiny.file_name()), b"").unwrap();
        assert!(!is_model_cached(WhisperModel::Tiny), "an empty file is not a model");

        std::fs::write(dir.join(WhisperModel::Tiny.file_name()), b"weights").unwrap();
        assert!(is_model_cached(WhisperModel::Tiny));

        remove_model(WhisperModel::Tiny).unwrap();
        assert!(!is_model_cached(WhisperModel::Tiny));

        unsafe { std::env::remove_var("SKILLREC_MODELS_DIR") };
        std::fs::remove_dir_all(&dir).ok();
    }
}
