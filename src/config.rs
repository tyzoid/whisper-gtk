use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use crate::services::default_audio_source as query_default_audio_source;

const DEFAULT_MAX_RECORDING_SECS: u32 = 30;
const MIN_MAX_RECORDING_SECS: u32 = 5;
const MAX_MAX_RECORDING_SECS: u32 = 180;

pub fn physical_core_count() -> u32 {
    physical_core_count_from_cpuinfo_internal(
        &std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default(),
    )
    .unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1)
    })
    .max(1)
}

#[cfg(test)]
pub(crate) fn physical_core_count_from_cpuinfo(cpuinfo: &str) -> Option<u32> {
    physical_core_count_from_cpuinfo_internal(cpuinfo)
}

fn physical_core_count_from_cpuinfo_internal(cpuinfo: &str) -> Option<u32> {
    let mut cores = std::collections::BTreeSet::new();
    let mut physical_id: Option<u32> = None;
    let mut core_id: Option<u32> = None;

    let finalize = |cores: &mut std::collections::BTreeSet<(u32, u32)>,
                    physical_id: &mut Option<u32>,
                    core_id: &mut Option<u32>| {
        if let (Some(physical_id), Some(core_id)) = (*physical_id, *core_id) {
            cores.insert((physical_id, core_id));
        }
        *physical_id = None;
        *core_id = None;
    };

    for line in cpuinfo.lines() {
        let line = line.trim();
        if line.is_empty() {
            finalize(&mut cores, &mut physical_id, &mut core_id);
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if key == "physical id" {
                physical_id = value.parse().ok();
            } else if key == "core id" {
                core_id = value.parse().ok();
            }
        }
    }
    finalize(&mut cores, &mut physical_id, &mut core_id);

    if cores.is_empty() {
        None
    } else {
        Some(cores.len() as u32)
    }
}

fn default_hotkey() -> String {
    "<F8>".to_string()
}

fn default_audio_source() -> Option<String> {
    if cfg!(test) {
        None
    } else {
        query_default_audio_source()
    }
}

fn default_output_mode() -> OutputMode {
    OutputMode::default()
}

fn default_max_recording_secs() -> u32 {
    DEFAULT_MAX_RECORDING_SECS
}

fn default_model_path() -> Option<String> {
    None
}

fn default_whisper_threads() -> u32 {
    physical_core_count()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub enum OutputMode {
    #[default]
    DirectTyping,
    ClipboardPaste,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppConfig {
    #[serde(default = "default_hotkey")]
    pub hotkey: String,
    #[serde(default = "default_audio_source")]
    pub audio_source: Option<String>,
    #[serde(default = "default_output_mode")]
    pub output_mode: OutputMode,
    #[serde(default = "default_max_recording_secs")]
    pub max_recording_secs: u32,
    #[serde(default = "default_model_path")]
    pub model_path: Option<String>,
    #[serde(default = "default_whisper_threads")]
    pub whisper_threads: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: default_hotkey(),
            audio_source: default_audio_source(),
            output_mode: default_output_mode(),
            max_recording_secs: default_max_recording_secs(),
            model_path: default_model_path(),
            whisper_threads: default_whisper_threads(),
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
            self.hotkey = default_hotkey();
        }
        self.max_recording_secs = self
            .max_recording_secs
            .clamp(MIN_MAX_RECORDING_SECS, MAX_MAX_RECORDING_SECS);
        if self.whisper_threads == 0 {
            self.whisper_threads = default_whisper_threads();
        }
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
