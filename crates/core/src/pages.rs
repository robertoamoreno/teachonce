//! The pages a recording visited, as facts rather than a model's paraphrase.
//!
//! Every navigation is in the events with its exact address. The describer
//! only ever mentions those addresses in prose ("the URL changed to the
//! release doc"), and the planner then decides from memory which pages to
//! pin. This module keeps the addresses themselves in play: it stamps them
//! onto the analysis steps by time, with no model involved, and compares the
//! set against a skill plan so a page the plan leaves out is shown to the
//! user instead of quietly disappearing.

use serde::{Deserialize, Serialize};

use crate::analysis::Analysis;
use crate::clock::AtMs;
use crate::events::{EventPayload, RecEvent};
use crate::skill::SkillPlan;
use crate::timeline::normalize_url;

/// One navigation, as the events recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Visit {
    pub at_ms: AtMs,
    pub app: String,
    pub url: String,
    /// The tab or window title seen with it, when the event carried one.
    pub title: String,
}

/// A page the recording visited, for the plan review.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VisitedPage {
    pub url: String,
    pub title: String,
    /// The analysis steps it was open during.
    pub step_ids: Vec<String>,
}

/// Every navigation in the recording, in order, with consecutive repeats of
/// the same page folded into one.
pub fn visits(events: &[RecEvent]) -> Vec<Visit> {
    let mut out: Vec<Visit> = Vec::new();
    for event in events {
        let (app, url, title) = match &event.payload {
            EventPayload::BrowserUrl { app, url, title, .. } => {
                (app, url, title.clone().unwrap_or_default())
            }
            EventPayload::AppActivate { app, url: Some(url), title, .. } => {
                (app, url, title.clone())
            }
            _ => continue,
        };
        let url = normalize_url(url.trim());
        if url.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.url == url => {
                if last.title.is_empty() {
                    last.title = title;
                }
            }
            _ => out.push(Visit { at_ms: event.t, app: app.clone(), url, title }),
        }
    }
    out
}

/// Stamp each analysis step with the pages open during it.
///
/// A page counts for a step when it was navigated to inside the step's span,
/// or was already showing when the step began and the step happened in that
/// browser. Steps without a start time are left as they are. Idempotent, so
/// it runs on every load and old analyses pick the pages up too.
pub fn stamp_step_urls(analysis: &mut Analysis, visits: &[Visit]) {
    let count = analysis.steps.len();
    for index in 0..count {
        let Some(start) = analysis.steps[index].start_ms else {
            continue;
        };
        // Models are loose with end times, so a step runs to the later of its
        // own end and the next step's start; the last step runs to the end.
        let next_start = analysis.steps.get(index + 1).and_then(|next| next.start_ms);
        let end = match (analysis.steps[index].end_ms, next_start) {
            (Some(a), Some(b)) => a.max(b),
            (Some(a), None) | (None, Some(a)) => a,
            (None, None) => AtMs::MAX,
        };
        let apps: Vec<String> =
            analysis.steps[index].apps.iter().map(|a| a.trim().to_lowercase()).collect();
        let in_app = |visit: &Visit| apps.is_empty() || apps.contains(&visit.app.trim().to_lowercase());

        let mut urls: Vec<String> = Vec::new();
        if let Some(open) = visits.iter().rev().find(|v| v.at_ms < start)
            && in_app(open)
        {
            urls.push(open.url.clone());
        }
        for visit in visits.iter().filter(|v| v.at_ms >= start && v.at_ms <= end) {
            if !urls.contains(&visit.url) {
                urls.push(visit.url.clone());
            }
        }
        analysis.steps[index].urls = urls;
    }
}

/// Pages the recording visited that the plan neither pins as a value nor
/// mentions in a step. The plan may be right to leave one out — a chat page
/// with a one-run id is not a fixed value — but the user should see the
/// omission now rather than discover it when the skill runs.
pub fn pages_not_in_plan(analysis: &Analysis, visits: &[Visit], plan: &SkillPlan) -> Vec<VisitedPage> {
    let mut pages: Vec<VisitedPage> = Vec::new();
    for visit in visits {
        if let Some(page) = pages.iter_mut().find(|p| p.url == visit.url) {
            if page.title.is_empty() {
                page.title = visit.title.clone();
            }
            continue;
        }
        let step_ids = analysis
            .steps
            .iter()
            .filter(|step| step.urls.contains(&visit.url))
            .map(|step| step.id.clone())
            .collect();
        pages.push(VisitedPage { url: visit.url.clone(), title: visit.title.clone(), step_ids });
    }
    pages.retain(|page| !plan_covers(plan, &page.url));
    pages
}

/// Does the plan account for this page: a value equal to it, a value that is
/// a parent path of it, or a step that spells the address out?
pub fn plan_covers(plan: &SkillPlan, url: &str) -> bool {
    let page = comparable(url);
    if page.is_empty() {
        return true;
    }
    if plan.values.iter().any(|value| covers(&comparable(&value.value), &page)) {
        return true;
    }
    plan.steps.iter().any(|step| comparable(&step.text).contains(&page))
        || comparable(&plan.summary).contains(&page)
        || comparable(&plan.generalization).contains(&page)
}

/// A value covers a page when it is the same page, or a parent path of it:
/// the Product Release Updates doc covers the sprint docs filed under it.
/// A bare site root covers nothing but itself, or every page would count.
fn covers(value: &str, page: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value == page {
        return true;
    }
    let has_path = value.split_once('/').is_some_and(|(_, path)| !path.is_empty());
    has_path
        && page.len() > value.len()
        && page.starts_with(value)
        && matches!(page.as_bytes()[value.len()], b'/' | b'?')
}

/// Lowercased, without scheme, `www.`, or a trailing slash or fragment, so
/// two spellings of one page compare equal.
fn comparable(url: &str) -> String {
    let mut s = url.trim().to_lowercase();
    for prefix in ["https://", "http://"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            s = rest.to_string();
            break;
        }
    }
    if let Some(rest) = s.strip_prefix("www.") {
        s = rest.to_string();
    }
    s.trim_end_matches('#').trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{AnalysisStep, Confidence};
    use crate::skill::{FixedValue, PlanStep, StepKind};

    fn nav(t: AtMs, url: &str, title: &str) -> RecEvent {
        RecEvent {
            seq: t as u64,
            t,
            epoch: 1_000 + t,
            source: "test".into(),
            payload: EventPayload::BrowserUrl {
                app: "Google Chrome".into(),
                url: url.into(),
                host: None,
                title: (!title.is_empty()).then(|| title.to_string()),
            },
        }
    }

    fn step(id: &str, start: AtMs, end: Option<AtMs>, apps: &[&str]) -> AnalysisStep {
        AnalysisStep {
            id: id.into(),
            title: id.into(),
            detail: String::new(),
            start_ms: Some(start),
            end_ms: end,
            apps: apps.iter().map(|a| a.to_string()).collect(),
            evidence: vec![],
            urls: vec![],
            confidence: Confidence::High,
        }
    }

    const BUILDER: &str = "https://www.askplace.ai/app?agentId=225";
    const CHAT: &str = "https://www.askplace.ai/app?chatId=757f2be3";
    const PARENT: &str = "https://app.clickup.com/8562814/v/dc/85a3y-98657";
    const RELEASES: &str = "https://app.clickup.com/8562814/v/dc/85a3y-98657/85a3y-56137";

    fn recording() -> (Vec<Visit>, Analysis) {
        let events = vec![
            nav(1_000, BUILDER, "Product Release Builder"),
            nav(1_500, BUILDER, ""),
            nav(50_000, CHAT, ""),
            nav(172_000, PARENT, "Product Release Updates"),
            nav(176_000, RELEASES, "09/04/2026 - Core Releases"),
        ];
        let visits = visits(&events);
        let mut analysis = Analysis {
            steps: vec![
                step("s1", 0, Some(30_000), &["Google Chrome"]),
                step("s2", 30_000, Some(30_500), &["Google Chrome"]), // sloppy end
                step("s3", 100_000, None, &["Terminal"]),
                step("s4", 170_000, None, &["Google Chrome"]),
            ],
            ..Default::default()
        };
        stamp_step_urls(&mut analysis, &visits);
        (visits, analysis)
    }

    #[test]
    fn navigations_are_listed_once_with_the_first_title_seen() {
        let (visits, _) = recording();
        let urls: Vec<&str> = visits.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(urls, vec![BUILDER, CHAT, PARENT, RELEASES]);
        assert_eq!(visits[0].title, "Product Release Builder");
        assert_eq!(visits[0].at_ms, 1_000);
    }

    #[test]
    fn steps_get_the_pages_open_during_them_from_the_events() {
        let (_, analysis) = recording();
        // Navigated to inside the span.
        assert_eq!(analysis.steps[0].urls, vec![BUILDER]);
        // Already open when the step began, plus the navigation before the
        // next step started — the model's own end time was too short.
        assert_eq!(analysis.steps[1].urls, vec![BUILDER, CHAT]);
        // A Terminal step does not inherit the browser's page.
        assert!(analysis.steps[2].urls.is_empty());
        // The last step runs to the end of the recording.
        assert_eq!(analysis.steps[3].urls, vec![CHAT, PARENT, RELEASES]);
    }

    #[test]
    fn stamping_is_idempotent_and_skips_steps_without_times() {
        let (visits, mut analysis) = recording();
        let before = analysis.clone();
        stamp_step_urls(&mut analysis, &visits);
        assert_eq!(analysis, before);

        let mut untimed = Analysis {
            steps: vec![AnalysisStep { start_ms: None, urls: vec!["kept".into()], ..step("x", 0, None, &[]) }],
            ..Default::default()
        };
        stamp_step_urls(&mut untimed, &visits);
        assert_eq!(untimed.steps[0].urls, vec!["kept"]);
    }

    fn plan_with(values: &[&str], text: &str) -> SkillPlan {
        SkillPlan {
            values: values
                .iter()
                .enumerate()
                .map(|(i, v)| FixedValue { id: format!("v{i}"), name: format!("v{i}"), value: v.to_string() })
                .collect(),
            steps: vec![PlanStep { title: "t".into(), text: text.into(), kind: StepKind::Action, tool: String::new() }],
            ..Default::default()
        }
    }

    #[test]
    fn pages_the_plan_leaves_out_are_listed_with_their_steps() {
        let (visits, analysis) = recording();
        let plan = plan_with(&[BUILDER], "Open {{v0}} and ask for the release notes.");
        let omitted = pages_not_in_plan(&analysis, &visits, &plan);
        let urls: Vec<&str> = omitted.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, vec![CHAT, PARENT, RELEASES]);
        let parent = &omitted[1];
        assert_eq!(parent.title, "Product Release Updates");
        assert_eq!(parent.step_ids, vec!["s4"]);
    }

    #[test]
    fn a_value_covers_its_own_page_and_the_pages_filed_under_it() {
        let (visits, analysis) = recording();
        // The parent doc as a value covers the sprint doc beneath it, and the
        // spelling differences a user introduces do not matter.
        let plan = plan_with(&[BUILDER, "http://App.ClickUp.com/8562814/v/dc/85a3y-98657/"], "");
        let urls: Vec<String> = pages_not_in_plan(&analysis, &visits, &plan).into_iter().map(|p| p.url).collect();
        assert_eq!(urls, vec![CHAT]);
        // A site root covers only itself.
        let root = plan_with(&["https://app.clickup.com"], "");
        assert!(!plan_covers(&root, PARENT));
        assert!(plan_covers(&root, "https://app.clickup.com/"));
    }

    #[test]
    fn a_step_that_spells_the_address_out_counts_as_covering_it() {
        let (visits, analysis) = recording();
        let plan = plan_with(&[BUILDER], &format!("Open {PARENT} and find this sprint's doc."));
        let urls: Vec<String> = pages_not_in_plan(&analysis, &visits, &plan).into_iter().map(|p| p.url).collect();
        assert_eq!(urls, vec![CHAT, RELEASES]);
    }
}
