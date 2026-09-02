//! The Tauri shell: commands, tray, global hotkey.
//!
//! This layer is deliberately thin. Every command is a small adapter over a
//! library crate, so the whole pipeline stays testable without a window — and so
//! a CLI or a daemon could be built on the same crates without touching any of
//! this.

mod commands;
mod state;

use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{Emitter, Manager};
use tauri_plugin_global_shortcut::{Code, GlobalShortcutExt, Modifiers, Shortcut, ShortcutState};

use state::AppState;

/// Toggle recording from anywhere: ⌘⇧R.
fn hotkey() -> Shortcut {
    Shortcut::new(Some(Modifiers::SUPER | Modifiers::SHIFT), Code::KeyR)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,skillrec=debug".into()),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    // Fire on press only; without this the hotkey toggles twice
                    // per keystroke and a recording starts and immediately stops.
                    if shortcut != &hotkey() || event.state() != ShortcutState::Pressed {
                        return;
                    }
                    let app = app.clone();
                    tauri::async_runtime::spawn(async move {
                        if let Err(err) = commands::toggle_recording(app.clone()).await {
                            tracing::warn!("hotkey toggle failed: {err}");
                        }
                    });
                })
                .build(),
        )
        .setup(|app| {
            app.manage(AppState::new(app.package_info().version.to_string()));

            // The app shipped as "Skill Recorder" before it was TeachOnce, under a
            // different identifier and therefore a different data folder. Adopt
            // that folder once, so existing recordings simply carry on.
            match skillrec_core::paths::adopt_legacy_data_dir() {
                Ok(Some(dir)) => tracing::info!(to = %dir.display(), "moved recordings from the Skill Recorder folder"),
                Ok(None) => {}
                Err(err) => tracing::warn!("could not adopt the Skill Recorder data folder: {err:#}"),
            }

            // Heal anything a crash or force-quit left half-written, before the
            // library is ever listed.
            match skillrec_recorder::recover_interrupted_sessions() {
                Ok(0) => {}
                Ok(count) => tracing::info!(count, "recovered interrupted recordings"),
                Err(err) => tracing::warn!("recovery pass failed: {err:#}"),
            }

            if let Err(err) = app.global_shortcut().register(hotkey()) {
                // Another app owning the hotkey is not fatal — the button and
                // the tray still work, so say so and carry on.
                tracing::warn!("could not register ⌘⇧R: {err}");
            }

            let toggle = MenuItem::with_id(app, "toggle", "Start Recording", true, None::<&str>)?;
            // Kept so `emit_status` can flip its label to "Stop Recording".
            if let Ok(mut slot) = app.state::<AppState>().tray_toggle.lock() {
                *slot = Some(toggle.clone());
            }
            let show = MenuItem::with_id(app, "show", "Open TeachOnce", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&toggle, &show, &quit])?;

            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "toggle" => {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let _ = commands::toggle_recording(app).await;
                        });
                    }
                    "show" => {
                        if let Some(window) = app.get_webview_window("main") {
                            let _ = window.show();
                            let _ = window.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing the window must not silently abandon a recording that is
            // still writing to disk, so it hides instead and the tray keeps it
            // reachable.
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::recorder_status,
            commands::start_recording,
            commands::stop_recording,
            commands::discard_recording,
            commands::toggle_recording,
            commands::set_microphone,
            commands::list_microphones,
            commands::permission_report,
            commands::request_screen_recording,
            commands::list_sessions,
            commands::load_session,
            commands::delete_session,
            commands::read_frame,
            commands::get_settings,
            commands::save_settings,
            commands::test_connection,
            commands::analyze_session,
            commands::revise_analysis,
            commands::edit_analysis,
            commands::debrief_questions,
            commands::answer_debrief,
            commands::plan_skill,
            commands::build_skill,
            commands::whisper_status,
            commands::download_whisper_model,
            commands::transcribe_session,
            commands::app_info,
        ])
        .run(tauri::generate_context!())
        .expect("error while running TeachOnce");
}

/// Push a status change to the UI, and keep the tray item's label honest.
pub fn emit_status(app: &tauri::AppHandle, status: &skillrec_recorder::RecorderStatus) {
    let _ = app.emit("recorder://status", status);

    let state = app.state::<AppState>();
    if let Ok(slot) = state.tray_toggle.lock()
        && let Some(item) = slot.as_ref()
    {
        let label = if status.recording { "Stop Recording" } else { "Start Recording" };
        if let Err(err) = item.set_text(label) {
            tracing::debug!("could not relabel the tray item: {err}");
        }
    }
}
