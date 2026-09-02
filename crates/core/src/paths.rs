//! Where everything lives on disk.
//!
//! ```text
//! ~/Library/Application Support/ai.teachonce.app/
//!   settings.json                  capture + LLM configuration
//!   models/ggml-<model>.bin        whisper weights (downloaded once)
//!   sessions/<id>/
//!     session.json                 metadata
//!     events.jsonl                 append-only event stream
//!     frames/frame_000123.jpg      retained screen stills
//!     audio/narration.wav          mic capture (only if you narrated)
//!     narration.json               on-device transcript
//!     bundle.json                  deterministic timeline
//!     description.md               deterministic narrative (LLM-free fallback)
//!     analysis.json                the LLM's intent + steps
//!     skill.json                   the built skill
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const QUALIFIER: &str = "ai";
const ORGANIZATION: &str = "teachonce";
const APPLICATION: &str = "app";

/// The identifier the app shipped under as "Skill Recorder". Its data folder is
/// adopted on first launch, so a rename never hides anyone's recordings.
const LEGACY: (&str, &str, &str) = ("com", "skillrecorder", "app");

/// Root application-data directory, honouring `SKILLREC_DATA_DIR` so tests and
/// dev runs never touch a real user's recordings.
pub fn data_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SKILLREC_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let dirs = directories::ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .context("could not resolve the application data directory")?;
    Ok(dirs.data_dir().to_path_buf())
}

/// Move the pre-rename data folder into place, once.
///
/// Returns the new location when something was moved. Does nothing under
/// `SKILLREC_DATA_DIR`, when there is no old folder, or when the new one
/// already exists — in that last case both are left alone rather than merged,
/// because merging two session trees silently is how recordings get lost.
pub fn adopt_legacy_data_dir() -> Result<Option<PathBuf>> {
    if std::env::var_os("SKILLREC_DATA_DIR").is_some() {
        return Ok(None);
    }
    let Some(old) = directories::ProjectDirs::from(LEGACY.0, LEGACY.1, LEGACY.2) else {
        return Ok(None);
    };
    move_data_dir(old.data_dir(), &data_root()?)
}

/// Rename `old` to `new` when `old` exists and `new` does not.
pub fn move_data_dir(old: &Path, new: &Path) -> Result<Option<PathBuf>> {
    if !old.is_dir() || new.exists() {
        return Ok(None);
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::rename(old, new)
        .with_context(|| format!("moving {} to {}", old.display(), new.display()))?;
    Ok(Some(new.to_path_buf()))
}

/// Directory holding every recording.
pub fn sessions_root() -> Result<PathBuf> {
    Ok(data_root()?.join("sessions"))
}

/// Directory holding downloaded Whisper weights.
pub fn models_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SKILLREC_MODELS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    Ok(data_root()?.join("models"))
}

/// The settings file.
pub fn settings_file() -> Result<PathBuf> {
    Ok(data_root()?.join("settings.json"))
}

/// Where built skills are installed so the target agent auto-loads them.
pub fn skills_root() -> Result<PathBuf> {
    if let Ok(dir) = std::env::var("SKILLREC_SKILLS_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let home = directories::UserDirs::new().context("could not resolve the home directory")?;
    Ok(home.home_dir().join(".config").join("skills"))
}

/// True when `id` is safe to join onto `sessions_root` as a single path segment.
///
/// Session ids reach this crate from the UI and from LLM tool arguments, so every
/// path built from one is validated here rather than at each call site.
pub fn is_valid_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Resolve a validated session directory.
pub fn session_dir(id: &str) -> Result<PathBuf> {
    anyhow::ensure!(is_valid_session_id(id), "invalid session id: {id:?}");
    Ok(sessions_root()?.join(id))
}

/// Resolve `relative` inside `root`, refusing anything that escapes it.
///
/// Used for every path that originates outside our own code — a frame filename
/// read back from a manifest, an export directory picked in a file dialog.
///
/// The check is **lexical**, not filesystem-based: the target frequently does not
/// exist yet (we are about to write it), and `canonicalize` on a temp directory
/// would compare a symlink-resolved root against an unresolved child and reject
/// perfectly valid paths.
pub fn resolve_within(root: &Path, relative: &str) -> Result<PathBuf> {
    use std::path::Component;

    let candidate = Path::new(relative);
    anyhow::ensure!(
        candidate.is_relative(),
        "path {relative:?} must be relative to its recording folder"
    );
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            // `..` is the whole attack: one of them in a frame filename read
            // back from a manifest would let a tampered session read anywhere.
            Component::ParentDir => {
                anyhow::bail!("path {relative:?} escapes its recording folder")
            }
            Component::RootDir | Component::Prefix(_) => {
                anyhow::bail!("path {relative:?} must be relative")
            }
        }
    }
    Ok(root.join(candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids_reject_traversal_and_separators() {
        assert!(is_valid_session_id("20260805-143000-a1b2c3d4"));
        assert!(!is_valid_session_id(".."));
        assert!(!is_valid_session_id("a/b"));
        assert!(!is_valid_session_id("a\\b"));
        assert!(!is_valid_session_id(""));
        assert!(!is_valid_session_id(&"x".repeat(65)));
    }

    #[test]
    fn the_old_data_folder_is_adopted_once_and_never_merged() {
        let base = std::env::temp_dir().join(format!("skillrec-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let old = base.join("com.skillrecorder.app");
        let new = base.join("ai.teachonce.app");

        // Nothing to move.
        assert!(move_data_dir(&old, &new).unwrap().is_none());

        std::fs::create_dir_all(old.join("sessions/abc")).unwrap();
        std::fs::write(old.join("sessions/abc/session.json"), "{}").unwrap();
        assert_eq!(move_data_dir(&old, &new).unwrap(), Some(new.clone()));
        assert!(new.join("sessions/abc/session.json").exists());
        assert!(!old.exists());

        // A second old folder appearing later is left where it is: both trees
        // exist, and a silent merge could clobber recordings.
        std::fs::create_dir_all(&old).unwrap();
        assert!(move_data_dir(&old, &new).unwrap().is_none());
        assert!(old.exists() && new.exists());

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn resolve_within_refuses_to_escape_the_root() {
        let root = Path::new("/tmp/skillrec-session");
        assert_eq!(
            resolve_within(root, "frames/frame_000001.jpg").unwrap(),
            root.join("frames/frame_000001.jpg")
        );
        // The target need not exist yet — we are usually about to create it.
        assert!(resolve_within(root, "frames/not-written-yet.jpg").is_ok());

        assert!(resolve_within(root, "../../etc/passwd").is_err());
        assert!(resolve_within(root, "frames/../../secrets").is_err());
        assert!(resolve_within(root, "/etc/passwd").is_err());
    }
}
