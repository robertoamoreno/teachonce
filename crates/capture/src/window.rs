//! Tracking the frontmost app, its window title, and — when it is a browser —
//! the page you are on.
//!
//! This is the primary signal. Most steps in a recording are fully explained by
//! "which app, which document, which URL", which is why the screen frames exist
//! only as enrichment for the cases where they are not.

use std::time::Duration;

use skillrec_core::events::EventPayload;
use skillrec_core::timeline::host_of;

use crate::collector::{Collector, CollectorContext};
use crate::url::{is_browser, MacUrlProvider, UrlProvider};

/// Base poll cadence.
const POLL: Duration = Duration::from_millis(1000);

/// Slower cadence while a browser is frontmost.
///
/// Not an optimisation for us — for the browser. Enumerating windows makes the
/// window server ask each app for its title, and doing that to a browser at 1 Hz
/// while it is also answering our Apple Events makes its UI stutter visibly.
const POLL_BROWSER: Duration = Duration::from_millis(1600);

/// Minimum spacing between URL reads while a browser stays frontmost. Catches
/// single-page-app navigations without hammering Apple Events.
const URL_MIN_INTERVAL: Duration = Duration::from_millis(1500);

/// The frontmost window, as far as we can see it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActiveWindow {
    pub app: String,
    pub title: String,
    pub pid: u32,
    pub bounds: Option<skillrec_core::events::WindowBounds>,
}

/// Windows smaller than this are chrome, not content: tooltips, popovers,
/// autofill dropdowns, the little status bubble Chrome shows over a link.
const MIN_REAL_WINDOW_AREA: u32 = 200 * 200;

/// Read the focused window. `None` when nothing is focused or the platform
/// refuses to say — which on macOS also means "Screen Recording is not granted",
/// since without it every title comes back empty.
///
/// Several windows can report themselves as focused at once. A browser showing
/// a link-target bubble or an autofill dropdown flags that 116×25 popover as
/// focused, and taking the first match yields an empty title and nonsense
/// bounds — so the largest titled window wins.
pub fn active_window() -> Option<ActiveWindow> {
    let windows = xcap::Window::all().ok()?;
    let focused = windows
        .into_iter()
        .filter(|w| w.is_focused().unwrap_or(false))
        .max_by_key(score_window)?;
    let app = focused.app_name().ok()?;
    if app.is_empty() {
        return None;
    }
    // Our own window is never part of the user's task — they only touch it to
    // press Start and Stop. Filtering by pid rather than by app name is what
    // makes this actually work: the bundled app reports "TeachOnce" but a
    // `cargo run` build reports "teachonce", and a name-based filter misses
    // one of them. The pid is exact and cannot drift.
    if focused.pid().unwrap_or(0) == std::process::id() {
        return None;
    }
    let bounds = match (focused.x(), focused.y(), focused.width(), focused.height()) {
        (Ok(x), Ok(y), Ok(width), Ok(height)) => Some(skillrec_core::events::WindowBounds {
            x: x as f64,
            y: y as f64,
            width: width as f64,
            height: height as f64,
        }),
        _ => None,
    };
    Some(ActiveWindow {
        app,
        title: focused.title().unwrap_or_default(),
        pid: focused.pid().unwrap_or(0),
        bounds,
    })
}

/// Rank a candidate focused window. A window with a real title always beats one
/// without, and beyond that bigger wins.
fn score_window(window: &xcap::Window) -> (u8, u32) {
    let area = window
        .width()
        .unwrap_or(0)
        .saturating_mul(window.height().unwrap_or(0));
    let titled = !window.title().unwrap_or_default().trim().is_empty();
    let substantial = area >= MIN_REAL_WINDOW_AREA;
    let tier = match (titled, substantial) {
        (true, true) => 3,
        (true, false) => 2,
        (false, true) => 1,
        (false, false) => 0,
    };
    (tier, area)
}

/// Emits `app.activate`, `app.title-change` and `browser.url`.
pub struct ActiveWindowCollector {
    capture_titles: bool,
    url_provider: Option<Box<dyn UrlProvider>>,
    state: WindowState,
}

/// The change-detection state, split out so it can be tested without a screen.
#[derive(Debug, Default)]
struct WindowState {
    last_app: String,
    last_title: String,
    last_url: String,
}

/// What a poll decided to emit.
#[derive(Debug, PartialEq)]
enum Transition {
    /// Nothing changed.
    None,
    /// A different app came to the front.
    Activated,
    /// Same app, new title.
    TitleChanged,
}

impl WindowState {
    fn observe(&mut self, app: &str, title: &str) -> Transition {
        if app != self.last_app {
            self.last_app = app.to_string();
            self.last_title = title.to_string();
            // A new app means the previous app's URL is no longer current;
            // clearing it makes the next URL in the new app emit even if it
            // happens to be the same page we saw earlier.
            self.last_url.clear();
            return Transition::Activated;
        }
        if title != self.last_title {
            self.last_title = title.to_string();
            return Transition::TitleChanged;
        }
        Transition::None
    }

    /// True when this URL is new and worth an event.
    fn observe_url(&mut self, url: &str) -> bool {
        if url.is_empty() || url == self.last_url {
            return false;
        }
        self.last_url = url.to_string();
        true
    }
}

impl ActiveWindowCollector {
    pub fn new(capture_titles: bool, capture_urls: bool) -> Self {
        Self {
            capture_titles,
            url_provider: capture_urls.then(|| Box::new(MacUrlProvider) as Box<dyn UrlProvider>),
            state: WindowState::default(),
        }
    }

    fn title_for(&self, window: &ActiveWindow) -> String {
        if self.capture_titles {
            window.title.clone()
        } else {
            String::new()
        }
    }
}

impl Collector for ActiveWindowCollector {
    fn name(&self) -> &'static str {
        "active-window"
    }

    fn run(&mut self, ctx: CollectorContext) {
        let mut last_url_read = std::time::Instant::now() - URL_MIN_INTERVAL;
        let mut warned = false;

        loop {
            let mut in_browser = false;

            if let Some(window) = active_window() {
                in_browser = is_browser(&window.app);
                let title = self.title_for(&window);

                match self.state.observe(&window.app, &title) {
                    Transition::Activated => ctx.publish(
                        "active-window",
                        EventPayload::AppActivate {
                            app: window.app.clone(),
                            title: title.clone(),
                            url: None,
                            host: None,
                            bundle_id: None,
                            pid: Some(window.pid),
                            bounds: window.bounds,
                        },
                    ),
                    Transition::TitleChanged => ctx.publish(
                        "active-window",
                        EventPayload::AppTitleChange {
                            app: window.app.clone(),
                            title: title.clone(),
                        },
                    ),
                    Transition::None => {}
                }

                // The URL read is throttled independently of the poll, so a
                // single-page-app navigation is still noticed without turning
                // every poll into an Apple Event.
                if let Some(provider) = self.url_provider.as_ref()
                    && provider.supports(&window.app)
                    && last_url_read.elapsed() >= URL_MIN_INTERVAL
                {
                    last_url_read = std::time::Instant::now();
                    if let Some(active) = provider.get(&window.app)
                        && self.state.observe_url(&active.url)
                    {
                        ctx.publish(
                            "active-window",
                            EventPayload::BrowserUrl {
                                app: window.app.clone(),
                                host: host_of(&active.url),
                                url: active.url,
                                title: active.title,
                            },
                        );
                    }
                }
            } else if !warned {
                warned = true;
                // Honest degradation: say it once, keep polling in case the user
                // grants the permission mid-recording.
                // Deliberately does not blame a permission. This also fires
                // when the screen is locked, during Mission Control, or when
                // every window is hidden — asserting a wrong cause sends people
                // to System Settings to fix something that is not broken.
                tracing::warn!(
                    "no focused window is visible; if this persists, check Screen Recording \
                     permission — it also happens while the screen is locked"
                );
            }

            if !ctx.sleep_or_stop(if in_browser { POLL_BROWSER } else { POLL }) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_apps_reports_an_activation() {
        let mut state = WindowState::default();
        assert_eq!(state.observe("Safari", "Docs"), Transition::Activated);
        assert_eq!(state.observe("Safari", "Docs"), Transition::None);
        assert_eq!(state.observe("Numbers", "Budget"), Transition::Activated);
    }

    #[test]
    fn a_new_title_in_the_same_app_is_not_an_activation() {
        let mut state = WindowState::default();
        state.observe("Safari", "Docs");
        assert_eq!(state.observe("Safari", "Pricing"), Transition::TitleChanged);
        assert_eq!(state.observe("Safari", "Pricing"), Transition::None);
    }

    #[test]
    fn repeated_urls_are_reported_once() {
        let mut state = WindowState::default();
        assert!(state.observe_url("https://example.com/a"));
        assert!(!state.observe_url("https://example.com/a"));
        assert!(state.observe_url("https://example.com/b"));
        assert!(!state.observe_url(""), "an empty read is not a navigation");
    }

    #[test]
    fn leaving_and_returning_to_a_page_reports_it_again() {
        // Otherwise a copy-paste round trip — browser, sheet, back to the same
        // page — silently loses the return leg of the journey.
        let mut state = WindowState::default();
        state.observe("Safari", "Docs");
        assert!(state.observe_url("https://example.com/a"));
        state.observe("Numbers", "Budget");
        state.observe("Safari", "Docs");
        assert!(state.observe_url("https://example.com/a"));
    }

    /// The scoring tiers, exercised without a screen. Mirrors what
    /// `score_window` computes from an `xcap::Window`.
    fn tier_of(titled: bool, area: u32) -> (u8, u32) {
        let substantial = area >= MIN_REAL_WINDOW_AREA;
        let tier = match (titled, substantial) {
            (true, true) => 3,
            (true, false) => 2,
            (false, true) => 1,
            (false, false) => 0,
        };
        (tier, area)
    }

    #[test]
    fn a_real_window_outranks_a_focused_popover() {
        // Observed live: Chrome flags its 116x25 link-target bubble as focused
        // alongside the actual browser window. Picking the bubble gives an empty
        // title and meaningless bounds for the whole recording.
        let popover = tier_of(false, 116 * 25);
        let browser = tier_of(true, 1440 * 900);
        assert!(browser > popover);
    }

    #[test]
    fn a_titled_small_window_still_beats_an_untitled_large_one() {
        // A find bar or a small utility window is real work; a large untitled
        // surface is usually a desktop or overlay layer.
        assert!(tier_of(true, 300 * 100) > tier_of(false, 2560 * 1440));
    }

    #[test]
    fn among_equals_the_larger_window_wins() {
        assert!(tier_of(true, 1440 * 900) > tier_of(true, 800 * 600));
    }

    #[test]
    fn titles_are_suppressed_when_the_source_is_disabled() {
        let collector = ActiveWindowCollector::new(false, false);
        let window = ActiveWindow { app: "Safari".into(), title: "Secret".into(), ..Default::default() };
        assert_eq!(collector.title_for(&window), "");
        assert!(collector.url_provider.is_none());

        let collector = ActiveWindowCollector::new(true, true);
        assert_eq!(collector.title_for(&window), "Secret");
        assert!(collector.url_provider.is_some());
    }
}
