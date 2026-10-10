//! Global keyboard shortcuts are desktop-only; keep their settings API available on iOS.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use tauri::AppHandle;

use crate::db::Db;

pub const SETTINGS_KEY: &str = "global_hotkeys";
pub(crate) static LAST_NONZERO_VOLUME: std::sync::atomic::AtomicI64 =
    std::sync::atomic::AtomicI64::new(100);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HotkeyAction {
    PlayPause,
    NextTrack,
    PrevTrack,
    VolumeUp,
    VolumeDown,
    MuteToggle,
    SeekForward,
    SeekBackward,
    ToggleShuffle,
    ToggleRepeat,
    ShowApp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeysConfig {
    pub enabled: bool,
    pub bindings: HashMap<HotkeyAction, String>,
}

impl Default for HotkeysConfig {
    fn default() -> Self {
        Self { enabled: false, bindings: HashMap::new() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyRegisterResult {
    pub success: bool,
    pub config: HotkeysConfig,
    pub errors: HashMap<HotkeyAction, String>,
}

pub struct HotkeysManager {
    config: Mutex<HotkeysConfig>,
}

impl HotkeysManager {
    pub fn new(config: HotkeysConfig) -> Self {
        Self { config: Mutex::new(config) }
    }

    pub fn get_config(&self) -> HotkeysConfig {
        self.config.lock().unwrap().clone()
    }

    pub fn apply_config(&self, _app: &AppHandle, config: HotkeysConfig) -> HotkeyRegisterResult {
        *self.config.lock().unwrap() = config.clone();
        HotkeyRegisterResult { success: false, config, errors: HashMap::new() }
    }
}

pub fn load_config(db: &Db) -> HotkeysConfig {
    db.get_setting(SETTINGS_KEY).and_then(|raw| serde_json::from_str(&raw).ok()).unwrap_or_default()
}

pub fn save_config(db: &Db, config: &HotkeysConfig) {
    if let Ok(raw) = serde_json::to_string(config) {
        db.set_setting(SETTINGS_KEY, &raw);
    }
}
