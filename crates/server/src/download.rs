//! `GET /api/sessions/{id}/skill.zip`: the built skill, ready to install.
//!
//! The archive holds `<name>/SKILL.md`, the same folder the desktop app's
//! Install writes, so unpacking it into `~/.claude/skills` installs the skill.

use std::io::{Cursor, Write};
use std::path::Path as FsPath;

use anyhow::{Context, Result};
use axum::extract::Path;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use skillrec_core::session::read_json;
use skillrec_core::skill::{render_skill_markdown, BuiltSkill};

pub async fn skill(Path(id): Path<String>) -> Response {
    let archive = skillrec_core::paths::sessions_root().and_then(|root| skill_archive(&root, &id));
    match archive {
        Ok((name, bytes)) => (
            [
                (header::CONTENT_TYPE, "application/zip".to_string()),
                (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{name}.zip\"")),
            ],
            bytes,
        )
            .into_response(),
        Err(err) => (StatusCode::NOT_FOUND, format!("{err:#}")).into_response(),
    }
}

/// The skill's folder name and its archive, or why there is none.
pub fn skill_archive(sessions_root: &FsPath, id: &str) -> Result<(String, Vec<u8>)> {
    anyhow::ensure!(skillrec_core::paths::is_valid_session_id(id), "invalid session id: {id:?}");
    let dir = sessions_root.join(id);
    let skill: BuiltSkill = read_json(&dir.join("skill.json"))
        .context("this recording has no built skill yet")?;
    // Names are slugs already; this only guards a hand-edited skill.json.
    let name: String = skill
        .name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    anyhow::ensure!(!name.is_empty(), "the built skill has no usable name");

    let mut buffer = Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        writer.start_file(format!("{name}/SKILL.md"), options).context("starting the archive")?;
        writer
            .write_all(render_skill_markdown(&skill).as_bytes())
            .context("writing SKILL.md")?;
        writer.finish().context("finishing the archive")?;
    }
    Ok((name, buffer.into_inner()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use skillrec_core::skill::FixedValue;
    use std::io::Read;

    #[test]
    fn the_archive_holds_the_rendered_skill_in_its_own_folder() {
        let dir = std::env::temp_dir().join(format!("teachonce-download-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sessions = dir.join("sessions");
        std::fs::create_dir_all(sessions.join("20260901-000000-dl000000")).unwrap();
        let skill = BuiltSkill {
            session_id: "20260901-000000-dl000000".into(),
            name: "sync-backlog".into(),
            description: "Sync the backlog".into(),
            body: "Open {{board}} and sync.".into(),
            values: vec![FixedValue { id: "board".into(), name: "board".into(), value: "Jira".into() }],
            ..Default::default()
        };
        skillrec_core::session::write_json(&sessions.join("20260901-000000-dl000000/skill.json"), &skill).unwrap();

        let (name, bytes) = skill_archive(&sessions, "20260901-000000-dl000000").unwrap();
        assert_eq!(name, "sync-backlog");
        let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).unwrap();
        assert_eq!(archive.len(), 1);
        let mut file = archive.by_index(0).unwrap();
        assert_eq!(file.name(), "sync-backlog/SKILL.md");
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();
        assert!(text.contains("name: sync-backlog"));
        assert!(text.contains("Open Jira and sync."), "values are substituted: {text}");

        // No skill yet, and an id that is not a session id at all.
        std::fs::create_dir_all(sessions.join("20260901-000001-dl000001")).unwrap();
        assert!(skill_archive(&sessions, "20260901-000001-dl000001").unwrap_err().to_string().contains("no built skill"));
        assert!(skill_archive(&sessions, "../etc").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
