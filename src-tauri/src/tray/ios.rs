//! iOS has no system tray; these adapters keep shared command/event code portable.

use tauri::{AppHandle, Emitter, Manager};

pub fn available() -> bool {
    false
}

pub fn set_available(_: bool) {}

pub fn init(_: &AppHandle) -> tauri::Result<()> {
    Ok(())
}

pub fn show_main(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
        set_main_visible(app, true);
    }
}

pub fn set_main_visible(app: &AppHandle, visible: bool) {
    let _ = app.emit_to("main", "ui-visible", visible);
}

pub fn set_playing(_: &AppHandle, _: bool) {}

pub fn set_icon(_: &AppHandle, _: &tauri::image::Image<'_>) {}
