//! Reading the frontmost browser's active tab URL.
//!
//! This is the richest signal that window titles cannot give you: a title says
//! "Pricing", a URL says *which* pricing page. macOS exposes it only through
//! Apple Events, which means `osascript`, which means a per-browser Automation
//! grant the first time.
//!
//! It is isolated behind [`UrlProvider`] for a reason beyond tidiness: each call
//! is a synchronous round-trip into another application's event loop, and a busy
//! or beachballing browser will make it hang. So it is called **on demand** —
//! only when a browser is frontmost and something changed — never on every poll,
//! and always with a hard timeout.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The active tab of a frontmost browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveUrl {
    pub url: String,
    pub title: Option<String>,
}

/// Which AppleScript dialect a browser speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserEngine {
    /// `active tab of front window`
    Chromium,
    /// `front document`
    WebKit,
}

/// Browsers whose scripting dictionary exposes `active tab of front window`.
const CHROMIUM_BROWSERS: &[&str] = &[
    "Google Chrome",
    "Google Chrome Canary",
    "Google Chrome Beta",
    "Google Chrome Dev",
    "Microsoft Edge",
    "Microsoft Edge Beta",
    "Microsoft Edge Dev",
    "Microsoft Edge Canary",
    "Brave Browser",
    "Brave Browser Beta",
    "Brave Browser Nightly",
    "Vivaldi",
    "Opera",
    "Opera GX",
    "Arc",
    "Chromium",
    "Yandex",
    "Comet",
    "Dia",
];

/// Browsers that use the WebKit scripting dictionary.
const WEBKIT_BROWSERS: &[&str] = &["Safari", "Safari Technology Preview", "Orion"];

/// Identify a frontmost app's dialect, or `None` if it is not a browser.
pub fn browser_engine(app: &str) -> Option<BrowserEngine> {
    if CHROMIUM_BROWSERS.contains(&app) {
        Some(BrowserEngine::Chromium)
    } else if WEBKIT_BROWSERS.contains(&app) {
        Some(BrowserEngine::WebKit)
    } else {
        None
    }
}

pub fn is_browser(app: &str) -> bool {
    browser_engine(app).is_some()
}

/// ASCII record separator — cannot appear in a URL and is vanishingly unlikely
/// in a page title, so it beats any printable delimiter for splitting the reply.
const SEP: char = '\u{1e}';

fn script_for(app: &str, engine: BrowserEngine) -> String {
    match engine {
        BrowserEngine::Chromium => format!(
            "tell application \"{app}\"\n\
             set _u to URL of active tab of front window\n\
             set _t to title of active tab of front window\n\
             return _u & (ASCII character 30) & _t\n\
             end tell"
        ),
        BrowserEngine::WebKit => format!(
            "tell application \"{app}\"\n\
             set _u to URL of front document\n\
             set _t to name of front document\n\
             return _u & (ASCII character 30) & _t\n\
             end tell"
        ),
    }
}

/// How long a browser gets to answer before we give up on this poll.
const OSASCRIPT_TIMEOUT: Duration = Duration::from_millis(800);

/// Run `osascript` with a hard timeout, killing it if the browser does not reply.
///
/// `Command` has no timeout, and this call reaches into another app's event loop
/// — without the kill, a hung browser would leave a stuck process behind on
/// every poll for the rest of the recording.
fn osascript(script: &str) -> Option<String> {
    let mut child = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + OSASCRIPT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                let mut out = String::new();
                child.stdout.take()?.read_to_string(&mut out).ok()?;
                return Some(out);
            }
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::debug!("osascript timed out; the browser is busy");
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => return None,
        }
    }
}

/// Parse the `url \x1e title` reply.
pub fn parse_reply(raw: &str) -> Option<ActiveUrl> {
    let raw = raw.trim_end_matches('\n');
    let (url, title) = match raw.split_once(SEP) {
        Some((url, title)) => (url.trim(), title.trim()),
        None => (raw.trim(), ""),
    };
    // AppleScript's own null. A browser with no front window returns this rather
    // than failing, so it must be treated as "no URL", not as a URL.
    if url.is_empty() || url == "missing value" {
        return None;
    }
    Some(ActiveUrl {
        url: url.to_string(),
        title: (!title.is_empty() && title != "missing value").then(|| title.to_string()),
    })
}

/// Reads the active tab URL of the frontmost browser.
pub trait UrlProvider: Send {
    fn supports(&self, app: &str) -> bool;
    /// Best effort — returns `None` on any failure, never blocks past the timeout.
    fn get(&self, app: &str) -> Option<ActiveUrl>;
}

/// macOS implementation, via Apple Events.
pub struct MacUrlProvider;

impl UrlProvider for MacUrlProvider {
    fn supports(&self, app: &str) -> bool {
        is_browser(app)
    }

    fn get(&self, app: &str) -> Option<ActiveUrl> {
        // Only ever called for an allow-listed browser name, so interpolating it
        // into the script carries no injection surface.
        let engine = browser_engine(app)?;
        parse_reply(&osascript(&script_for(app, engine))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browsers_are_matched_to_their_dialect() {
        assert_eq!(browser_engine("Safari"), Some(BrowserEngine::WebKit));
        assert_eq!(browser_engine("Google Chrome"), Some(BrowserEngine::Chromium));
        assert_eq!(browser_engine("Arc"), Some(BrowserEngine::Chromium));
        assert_eq!(browser_engine("Numbers"), None);
        // Matching is exact — a lookalike app name must not be scripted.
        assert_eq!(browser_engine("safari"), None);
    }

    #[test]
    fn each_dialect_gets_its_own_script() {
        assert!(script_for("Safari", BrowserEngine::WebKit).contains("front document"));
        assert!(script_for("Arc", BrowserEngine::Chromium).contains("active tab of front window"));
        assert!(script_for("Arc", BrowserEngine::Chromium).contains("tell application \"Arc\""));
    }

    #[test]
    fn replies_split_on_the_record_separator() {
        let reply = format!("https://example.com/pricing{SEP}Pricing — Example\n");
        let parsed = parse_reply(&reply).unwrap();
        assert_eq!(parsed.url, "https://example.com/pricing");
        assert_eq!(parsed.title.as_deref(), Some("Pricing — Example"));
    }

    #[test]
    fn a_url_containing_no_separator_still_parses() {
        let parsed = parse_reply("https://example.com\n").unwrap();
        assert_eq!(parsed.url, "https://example.com");
        assert!(parsed.title.is_none());
    }

    #[test]
    fn applescripts_null_is_not_mistaken_for_a_url() {
        // A browser with no window open answers "missing value"; treating that
        // as a URL would emit a navigation event for a window that isn't there.
        assert!(parse_reply("missing value").is_none());
        assert!(parse_reply(&format!("missing value{SEP}missing value")).is_none());
        assert!(parse_reply("").is_none());
        assert!(parse_reply("   \n").is_none());
    }

    #[test]
    fn a_missing_title_is_none_rather_than_an_empty_string() {
        let parsed = parse_reply(&format!("https://example.com{SEP}missing value")).unwrap();
        assert!(parsed.title.is_none());
        let parsed = parse_reply(&format!("https://example.com{SEP}   ")).unwrap();
        assert!(parsed.title.is_none());
    }

    #[test]
    fn osascript_returns_none_rather_than_hanging_on_a_bad_script() {
        // Exercises the real timeout path: an invalid script exits non-zero fast.
        assert!(osascript("this is not applescript").is_none());
    }
}
