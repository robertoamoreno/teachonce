// Prevents a console window from opening alongside the app on Windows release
// builds. Harmless on macOS, kept so the crate stays portable.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    skill_recorder_lib::run()
}
