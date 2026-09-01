//! macOS permission checks.
//!
//! These exist so the app can say "titles will be blank until you grant Screen
//! Recording" *before* you record a session, rather than after you have already
//! done the work and got an empty timeline. Every check is a preflight: it
//! reports state without triggering a prompt, except [`request_screen_recording`]
//! which explicitly asks.

use serde::{Deserialize, Serialize};

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// Reports whether this process can capture the screen, without prompting.
    fn CGPreflightScreenCaptureAccess() -> bool;
    /// Prompts for Screen Recording. Returns immediately; macOS requires a
    /// relaunch before a newly granted permission takes effect.
    fn CGRequestScreenCaptureAccess() -> bool;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Reports whether this process is trusted for Accessibility.
    fn AXIsProcessTrusted() -> bool;
}

/// State of one permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionState {
    Granted,
    Denied,
    /// Not applicable on this platform.
    NotRequired,
}

impl From<bool> for PermissionState {
    fn from(granted: bool) -> Self {
        if granted { Self::Granted } else { Self::Denied }
    }
}

/// What each permission unlocks, for the readiness panel.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionReport {
    pub screen_recording: PermissionState,
    pub accessibility: PermissionState,
    /// Every consequence of a missing permission, in plain language.
    pub warnings: Vec<String>,
}

impl PermissionReport {
    /// Can a recording produce anything useful at all?
    ///
    /// True even with everything denied: app-switch tracking and the clipboard
    /// need no permission, so a recording is still worth making. The warnings
    /// say what will be missing.
    pub fn can_record(&self) -> bool {
        true
    }

    /// Is every signal available?
    pub fn fully_granted(&self) -> bool {
        self.warnings.is_empty()
    }
}

/// Screen Recording — needed for frames *and*, on modern macOS, for window
/// titles. That coupling is why a denial costs more than just screenshots.
pub fn screen_recording() -> PermissionState {
    unsafe { CGPreflightScreenCaptureAccess() }.into()
}

/// Accessibility — needed for reliable window geometry and focus.
pub fn accessibility() -> PermissionState {
    unsafe { AXIsProcessTrusted() }.into()
}

/// Prompt for Screen Recording. macOS shows the dialog once per app; afterwards
/// the user must toggle it in System Settings, so the UI should link there too.
pub fn request_screen_recording() -> bool {
    unsafe { CGRequestScreenCaptureAccess() }
}

/// Build the readiness report.
pub fn report() -> PermissionReport {
    let screen = screen_recording();
    let ax = accessibility();
    let mut warnings = Vec::new();

    if screen != PermissionState::Granted {
        warnings.push(
            "Screen Recording is off: no screen frames will be captured, and window titles \
             will be blank. Grant it in System Settings → Privacy & Security → Screen Recording."
                .into(),
        );
    }
    if ax != PermissionState::Granted {
        warnings.push(
            "Accessibility is off: window focus and geometry may be unreliable. Grant it in \
             System Settings → Privacy & Security → Accessibility."
                .into(),
        );
    }
    // Deliberately not preflighted: the Automation grant is per-browser and macOS
    // only offers it at the moment of the first Apple Event, so there is nothing
    // to check until the user records with a browser open.
    warnings.push(
        "Browser URLs need a one-time Automation grant per browser. macOS asks the first time \
         you record with that browser in front."
            .into(),
    );

    PermissionReport { screen_recording: screen, accessibility: ax, warnings }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_checks_do_not_panic_or_prompt() {
        // Whatever the answer is on this machine, it must be one of the two real
        // states — never a crash from the FFI boundary.
        assert!(matches!(
            screen_recording(),
            PermissionState::Granted | PermissionState::Denied
        ));
        assert!(matches!(accessibility(), PermissionState::Granted | PermissionState::Denied));
    }

    #[test]
    fn recording_is_always_possible_even_with_nothing_granted() {
        // App switches and the clipboard need no permission at all, so the app
        // must never refuse to record — it warns instead.
        let report = report();
        assert!(report.can_record());
        assert!(!report.warnings.is_empty(), "the Automation note is always present");
    }

    #[test]
    fn a_denial_explains_its_consequence_not_just_its_name() {
        let report = report();
        if report.screen_recording == PermissionState::Denied {
            let warning = report.warnings.iter().find(|w| w.contains("Screen Recording")).unwrap();
            assert!(warning.contains("titles"), "the title coupling must be spelled out");
        }
    }
}
