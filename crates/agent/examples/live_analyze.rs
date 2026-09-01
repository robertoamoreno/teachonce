//! Run the real describer against a real, configured endpoint.
//!
//!     cargo run -p skillrec-agent --example live_analyze
//!
//! Builds a synthetic recording — a plausible copy-a-price-into-a-sheet session —
//! writes it to a temp folder, and runs the actual `Describer` against whatever
//! `Settings` points at. An example rather than a test because it needs a live
//! model server and takes as long as that server takes.

use std::path::PathBuf;

use skillrec_agent::{Describer, SessionData};
use skillrec_core::config::Settings;
use skillrec_core::events::{EventPayload, RecEvent};
use skillrec_core::session::{write_json, SessionMeta};

fn event(seq: u64, t: i64, payload: EventPayload) -> RecEvent {
    RecEvent { seq, t, epoch: 1_700_000_000_000 + t, source: "fixture".into(), payload }
}

fn activate(seq: u64, t: i64, app: &str, title: &str) -> RecEvent {
    event(
        seq,
        t,
        EventPayload::AppActivate {
            app: app.into(),
            title: title.into(),
            url: None,
            host: None,
            bundle_id: None,
            pid: Some(100),
            bounds: None,
        },
    )
}

fn url(seq: u64, t: i64, app: &str, url: &str, title: &str) -> RecEvent {
    event(
        seq,
        t,
        EventPayload::BrowserUrl {
            app: app.into(),
            url: url.into(),
            host: None,
            title: Some(title.into()),
        },
    )
}

fn copied(seq: u64, t: i64, text: &str) -> RecEvent {
    event(
        seq,
        t,
        EventPayload::ClipboardChange {
            formats: vec!["text/plain".into()],
            length: text.len(),
            hash: "h".into(),
            text_preview: Some(text.into()),
        },
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    let settings = Settings::load()?;
    println!("endpoint: {}", settings.llm.base_url);
    println!("model:    {}", settings.llm.model);
    println!("vision:   {}\n", settings.llm.vision);

    let dir: PathBuf = std::env::temp_dir().join("skillrec-live-analyze");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    let meta = SessionMeta {
        id: "livefixture".into(),
        started_at: 1_700_000_000_000,
        stopped_at: Some(1_700_000_000_000 + 48_000),
        platform: "macos".into(),
        app_version: "example".into(),
        narrated: false,
        title: None,
    };
    write_json(&dir.join("session.json"), &meta)?;

    // A recognisable task: look up two vendors' enterprise pricing, copy each,
    // and paste them into a spreadsheet. Includes one off-task detour (a recipe
    // page) that a good describer should leave out.
    let events = vec![
        activate(1, 0, "Safari", "Acme — Pricing"),
        url(2, 500, "Safari", "https://acme.example.com/pricing", "Acme — Pricing"),
        copied(3, 6_000, "Enterprise — $499/mo, unlimited seats"),
        url(4, 12_000, "Safari", "https://recipes.example.com/lasagne", "Best Lasagne"),
        activate(5, 16_000, "Numbers", "Vendor Costs.numbers"),
        activate(6, 22_000, "Safari", "Globex — Plans"),
        url(7, 22_500, "Safari", "https://globex.example.com/plans", "Globex — Plans"),
        copied(8, 28_000, "Enterprise — $650/mo, 200 seats included"),
        activate(9, 34_000, "Numbers", "Vendor Costs.numbers"),
    ];
    let lines: String = events
        .iter()
        .map(|e| format!("{}\n", serde_json::to_string(e).unwrap()))
        .collect();
    std::fs::write(dir.join("events.jsonl"), lines)?;

    let data = SessionData::load(&dir)?;
    println!("fixture: {} events → {} steps\n", data.events.len(), data.bundle.steps.len());

    let started = std::time::Instant::now();
    let analysis = Describer::new(settings.llm)
        .analyze(data, &|p| println!("  [{}] {}", p.phase, p.message))
        .await?;

    println!("\n=== analysis in {:.1}s ===", started.elapsed().as_secs_f64());
    println!("title:      {}", analysis.title);
    println!("intent:     {}", analysis.intent);
    println!("confidence: {:?}", analysis.intent_confidence);
    println!("rationale:  {}", analysis.intent_rationale);
    println!("\nsteps:");
    for step in &analysis.steps {
        println!("  {} — {}", step.id, step.title);
        if !step.detail.is_empty() {
            println!("      {}", step.detail);
        }
    }

    let mentions_recipe = analysis
        .steps
        .iter()
        .any(|s| s.title.to_lowercase().contains("lasagne") || s.detail.to_lowercase().contains("lasagne"));
    println!(
        "\noff-task detour pruned: {}",
        if mentions_recipe { "NO — the recipe page survived" } else { "yes" }
    );

    std::fs::remove_dir_all(&dir).ok();
    Ok(())
}
