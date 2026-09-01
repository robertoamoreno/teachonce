//! The builder's output contract: a plan the user reviews, then a `SKILL.md`.
//!
//! Two ideas carry this module.
//!
//! **Fixed values are tokens.** Anything that is the same on every run — a
//! canonical URL, a repo slug, an API constant — is pulled out of the prose into
//! a named value and referenced as `{{id}}`. The user edits it once in a form
//! field and it substitutes everywhere. Values that vary run-to-run must *not*
//! become tokens; they become instructions telling the agent to go find them.
//!
//! **Steps are typed by consequence.** A `calculation` reads, derives, or
//! decides. An `action` changes the world — submits, sends, deletes, pays. The
//! distinction is surfaced in the review UI because those are the steps worth
//! looking at twice before you let something run on a schedule.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// A literal that is constant across runs, referenced from step text as `{{id}}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FixedValue {
    /// Short snake_case key, e.g. `backlog_url`.
    pub id: String,
    /// Human label for the editable field, e.g. "Blog Backlog URL".
    pub name: String,
    /// The literal itself.
    pub value: String,
}

/// Does this step observe, or does it change something?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    /// Reads, derives, filters, formats. No external side effect.
    #[default]
    Calculation,
    /// Submits, sends, creates, edits, deletes, pays.
    Action,
}

/// One step of the generalized procedure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanStep {
    pub title: String,
    pub text: String,
    #[serde(default)]
    pub kind: StepKind,
    /// The native capability this step uses, e.g. `bash(gh *)`, `web_fetch`.
    #[serde(default)]
    pub tool: String,
}

/// What the model proposes and the user edits before anything is written.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillPlan {
    /// Slug used for the folder and the frontmatter `name`.
    pub name: String,
    /// Human title shown in the review UI.
    #[serde(default)]
    pub title: String,
    /// The frontmatter `description` — the trigger the agent matches on.
    pub description: String,
    /// One paragraph on what this skill does.
    #[serde(default)]
    pub summary: String,
    /// How the single recorded run was generalized.
    #[serde(default)]
    pub generalization: String,
    #[serde(default)]
    pub values: Vec<FixedValue>,
    #[serde(default)]
    pub steps: Vec<PlanStep>,
    /// Tool patterns the skill is permitted to use.
    #[serde(default)]
    pub allowed_tools: Vec<String>,
}

/// The finished artifact.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuiltSkill {
    pub session_id: String,
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// The SKILL.md instructions, still containing `{{id}}` tokens.
    pub body: String,
    /// The values those tokens resolve to.
    #[serde(default)]
    pub values: Vec<FixedValue>,
    #[serde(default)]
    pub model: String,
}

/// What the model sends to `submit_skill`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillSubmission {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    pub body: String,
}

/// Turn any label into a safe directory / frontmatter name.
pub fn slugify(input: &str) -> String {
    let mut out = String::new();
    let mut last_dash = true;
    for ch in input.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash && !out.is_empty() {
            out.push('-');
            last_dash = true;
        }
    }
    let slug = out.trim_matches('-').to_string();
    if slug.is_empty() {
        "recorded-skill".to_string()
    } else {
        slug.chars().take(64).collect()
    }
}

/// Substitute every `{{id}}` for its value.
pub fn substitute(body: &str, values: &[FixedValue]) -> String {
    let map: BTreeMap<&str, &str> =
        values.iter().map(|v| (v.id.as_str(), v.value.as_str())).collect();
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let key = after[..end].trim();
                match map.get(key) {
                    Some(value) => out.push_str(value),
                    // An unknown token is left verbatim rather than blanked: a
                    // visible `{{typo}}` in the output is a bug you can see,
                    // an empty string in a shell command is one you cannot.
                    None => out.push_str(&rest[start..start + 2 + end + 2]),
                }
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Tokens referenced by the body that no value defines.
pub fn unresolved_tokens(body: &str, values: &[FixedValue]) -> Vec<String> {
    let known: Vec<&str> = values.iter().map(|v| v.id.as_str()).collect();
    let mut missing = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        let key = after[..end].trim().to_string();
        if !key.is_empty() && !known.contains(&key.as_str()) && !missing.contains(&key) {
            missing.push(key);
        }
        rest = &after[end + 2..];
    }
    missing
}

/// Render the final `SKILL.md`, values already substituted.
pub fn render_skill_markdown(skill: &BuiltSkill) -> String {
    let mut out = String::from("---\n");
    out.push_str(&format!("name: {}\n", skill.name));
    out.push_str(&format!("description: {}\n", yaml_scalar(&skill.description)));
    if !skill.allowed_tools.is_empty() {
        out.push_str(&format!("allowed-tools: {}\n", skill.allowed_tools.join(", ")));
    }
    out.push_str("---\n\n");
    out.push_str(substitute(&skill.body, &skill.values).trim());
    out.push('\n');
    out
}

/// Quote a YAML scalar when it contains anything that would change the parse.
fn yaml_scalar(value: &str) -> String {
    let value = value.replace('\n', " ");
    let needs_quotes = value.starts_with(['&', '*', '!', '|', '>', '%', '@', '`', '"', '\'', '[', '{', '#', '-', '?'])
        || value.contains(": ")
        || value.ends_with(':');
    if needs_quotes {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values() -> Vec<FixedValue> {
        vec![
            FixedValue {
                id: "backlog_url".into(),
                name: "Backlog URL".into(),
                value: "https://example.com/backlog".into(),
            },
            FixedValue { id: "repo".into(), name: "Repo".into(), value: "acme/widgets".into() },
        ]
    }

    #[test]
    fn tokens_substitute_everywhere_they_appear() {
        let body = "Open {{backlog_url}}, then run `gh issue list -R {{repo}}`, then reopen {{backlog_url}}.";
        let out = substitute(body, &values());
        assert_eq!(
            out,
            "Open https://example.com/backlog, then run `gh issue list -R acme/widgets`, then reopen https://example.com/backlog."
        );
    }

    #[test]
    fn an_unknown_token_stays_visible_instead_of_becoming_an_empty_string() {
        // Silently blanking this would produce `gh issue list -R ` — a command
        // that fails confusingly, or worse, succeeds against the wrong target.
        let out = substitute("run gh issue list -R {{typo}}", &values());
        assert_eq!(out, "run gh issue list -R {{typo}}");
        assert_eq!(unresolved_tokens("a {{typo}} b {{repo}}", &values()), vec!["typo"]);
    }

    #[test]
    fn unterminated_braces_do_not_panic_or_eat_the_body() {
        assert_eq!(substitute("tail {{unclosed", &values()), "tail {{unclosed");
        assert!(unresolved_tokens("tail {{unclosed", &values()).is_empty());
        assert_eq!(substitute("", &values()), "");
    }

    #[test]
    fn whitespace_inside_a_token_is_tolerated() {
        assert_eq!(substitute("{{ repo }}", &values()), "acme/widgets");
    }

    #[test]
    fn slugs_are_safe_directory_names() {
        assert_eq!(slugify("Extract Invoice Data!"), "extract-invoice-data");
        assert_eq!(slugify("  multiple   spaces  "), "multiple-spaces");
        assert_eq!(slugify("../../etc/passwd"), "etc-passwd");
        assert_eq!(slugify("???"), "recorded-skill");
        assert!(slugify(&"x".repeat(200)).len() <= 64);
    }

    #[test]
    fn rendered_markdown_has_frontmatter_and_resolved_tokens() {
        let skill = BuiltSkill {
            session_id: "s".into(),
            name: "sync-backlog".into(),
            description: "Sync the backlog. Use when asked to refresh issues.".into(),
            allowed_tools: vec!["Bash(gh *)".into(), "web_fetch".into()],
            body: "Fetch {{backlog_url}} and file issues in {{repo}}.".into(),
            values: values(),
            model: "gpt-4o".into(),
        };
        let md = render_skill_markdown(&skill);
        assert!(md.starts_with("---\nname: sync-backlog\n"));
        assert!(md.contains("allowed-tools: Bash(gh *), web_fetch"));
        assert!(md.contains("Fetch https://example.com/backlog and file issues in acme/widgets."));
        assert!(!md.contains("{{"));
    }

    #[test]
    fn descriptions_that_would_break_yaml_are_quoted() {
        assert_eq!(yaml_scalar("Plain description"), "Plain description");
        assert!(yaml_scalar("Use when: you need it").starts_with('"'));
        assert!(yaml_scalar("- dashed start").starts_with('"'));
        assert_eq!(yaml_scalar("say \"hi\""), "say \"hi\"");
    }
}
