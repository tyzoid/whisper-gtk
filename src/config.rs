use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

const DEFAULT_MAX_RECORDING_SECS: u32 = 30;
const MIN_MAX_RECORDING_SECS: u32 = 5;
const MAX_MAX_RECORDING_SECS: u32 = 180;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    DirectTyping,
    ClipboardPaste,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    pub hotkey: String,
    pub audio_source: Option<String>,
    pub output_mode: OutputMode,
    pub max_recording_secs: u32,
    pub model_path: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: "<F8>".to_string(),
            audio_source: None,
            output_mode: OutputMode::DirectTyping,
            max_recording_secs: DEFAULT_MAX_RECORDING_SECS,
            model_path: None,
        }
    }
}

impl AppConfig {
    pub fn path() -> PathBuf {
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("whisper-gtk").join("config.json")
    }

    pub fn normalize(&mut self) {
        if self.hotkey.trim().is_empty() {
            self.hotkey = "<F8>".to_string();
        }
        self.max_recording_secs = self
            .max_recording_secs
            .clamp(MIN_MAX_RECORDING_SECS, MAX_MAX_RECORDING_SECS);
    }

    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Self {
        match fs::read_to_string(path) {
            Ok(contents) => {
                let mut config: Self = serde_json::from_str(&contents).unwrap_or_default();
                config.normalize();
                config
            }
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> io::Result<()> {
        self.save_to(&Self::path())
    }

    pub fn save_to(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let serialized =
            serde_json::to_string_pretty(self).map_err(|err| io::Error::other(err.to_string()))?;
        fs::write(path, serialized)
    }
}
