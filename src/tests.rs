use crate::config::{physical_core_count_from_cpuinfo, AppConfig, OutputMode};
use crate::services::{
    build_recording_command, hotkey_matches, keycode_is_down, list_whisper_models_in,
    overlay_position_for_monitor, parse_x11_hotkey, raw_to_wav, AudioStats, Hotkey,
    MonitorGeometry, RecordingGeneration,
};
use crate::ui::{audio_source_selection_to_config, output_mode_selection_to_config, WaveformState};
use gtk::gdk;
use std::fs;
use std::time::{SystemTime, UNIX_EPOCH};

fn command_args(command: &std::process::Command) -> Vec<String> {
    command
        .get_args()
        .map(|arg| arg.to_string_lossy().to_string())
        .collect()
}

#[test]
fn config_roundtrip() {
    let cfg = AppConfig {
        hotkey: "<Ctrl><Alt>F8".to_string(),
        audio_source: Some("alsa_input".to_string()),
        output_mode: OutputMode::ClipboardPaste,
        max_recording_secs: 42,
        model_path: None,
        whisper_threads: 8,
    };
    let text = serde_json::to_string(&cfg).unwrap();
    let decoded: AppConfig = serde_json::from_str(&text).unwrap();
    assert_eq!(cfg, decoded);
}

#[test]
fn config_persists_to_custom_path() {
    let dir = std::env::temp_dir().join("whisper-gtk-config-test");
    let _ = fs::create_dir_all(&dir);
    let path = dir.join("config.json");
    let cfg = AppConfig {
        hotkey: "<Super>F8".to_string(),
        audio_source: Some("default-source".to_string()),
        output_mode: OutputMode::ClipboardPaste,
        max_recording_secs: 55,
        model_path: None,
        whisper_threads: 8,
    };
    cfg.save_to(&path).unwrap();
    let loaded = AppConfig::load_from(&path);
    assert_eq!(loaded, cfg);
    let _ = fs::remove_file(path);
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn config_normalize_clamps_max_recording_duration() {
    let mut too_low = AppConfig {
        max_recording_secs: 1,
        ..AppConfig::default()
    };
    too_low.normalize();
    assert_eq!(too_low.max_recording_secs, 5);

    let mut too_high = AppConfig {
        max_recording_secs: 999,
        ..AppConfig::default()
    };
    too_high.normalize();
    assert_eq!(too_high.max_recording_secs, 180);
}

#[test]
fn physical_core_count_parser_counts_unique_cores() {
    let cpuinfo = "\
processor   : 0
physical id : 0
core id     : 0

processor   : 1
physical id : 0
core id     : 1

processor   : 2
physical id : 0
core id     : 0
";
    assert_eq!(physical_core_count_from_cpuinfo(cpuinfo), Some(2));
}

#[test]
fn temp_recording_files_are_private() {
    use std::os::unix::fs::PermissionsExt;

    let raw = std::env::temp_dir().join("whisper-gtk-private-test.raw");
    let wav = std::env::temp_dir().join("whisper-gtk-private-test.wav");
    let _ = fs::remove_file(&raw);
    let _ = fs::remove_file(&wav);
    fs::write(&raw, vec![0u8; 320]).unwrap();
    raw_to_wav(&raw, &wav).unwrap();
    let mode = fs::metadata(&wav).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let _ = fs::remove_file(raw);
    let _ = fs::remove_file(wav);
}

#[test]
fn recording_generation_ignores_stale_timeout() {
    let mut state = RecordingGeneration::default();
    let first = state.next();
    assert_eq!(state.current(), Some(first));
    assert!(state.stop_if_current(first));
    let second = state.next();
    assert_ne!(first, second);
    assert!(!state.stop_if_current(first));
    assert!(state.stop_if_current(second));
    assert_eq!(state.current(), None);
}

#[test]
fn hotkey_match_ignores_lock_masks() {
    let hotkey = Hotkey {
        key: gdk::Key::F8,
        mods: gdk::ModifierType::CONTROL_MASK,
    };
    let key = gdk::Key::F8;
    assert!(hotkey_matches(
        hotkey,
        key,
        gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::LOCK_MASK
    ));
}

#[test]
fn x11_hotkey_parser_accepts_gtk_accelerators_without_gtk_threading() {
    assert!(parse_x11_hotkey("<F8>").is_some());
    assert!(parse_x11_hotkey("<Control><Alt>F8").is_some());
    assert!(parse_x11_hotkey("Control+Alt+F8").is_some());
    assert!(parse_x11_hotkey("").is_none());
}

#[test]
fn x11_keymap_helper_detects_pressed_keycodes() {
    let mut keymap = [0i8; 32];
    keymap[4] = 1 << 1;
    assert!(keycode_is_down(&keymap, 33));
    assert!(!keycode_is_down(&keymap, 34));
}

#[test]
fn overlay_position_targets_monitor_centerline() {
    let monitor = MonitorGeometry {
        x: 100,
        y: 200,
        width: 1000,
        height: 800,
    };
    assert_eq!(overlay_position_for_monitor(monitor, 200, 40), (500, 860));
}

#[test]
fn waveform_state_scrolls_level_history() {
    let mut state = WaveformState::default();
    let initial = state.bar_heights(11);
    assert_eq!(initial.len(), 11);
    assert!(initial.iter().all(|height| (*height - 0.16).abs() < 1e-6));

    state.set_level(1.0);
    state.advance();
    let first = state.bar_heights(11);
    assert!((first[10] - 0.90).abs() < 1e-6);
    assert!((first[9] - 0.16).abs() < 1e-6);

    state.set_level(0.5);
    state.advance();
    let second = state.bar_heights(11);
    assert!((second[10] - 0.53).abs() < 1e-6);
    assert!((second[9] - 0.90).abs() < 1e-6);
}

#[test]
fn audio_gate_discards_short_or_silent_recordings() {
    let mut carry = None;
    let mut short_loud = AudioStats::default();
    short_loud.ingest_bytes(&pcm_samples(0.5, 1600), &mut carry);
    assert!(!short_loud.should_transcribe());

    let mut silent = AudioStats::default();
    let mut carry = None;
    silent.ingest_bytes(&pcm_samples(0.0, 8000), &mut carry);
    assert!(!silent.should_transcribe());
}

#[test]
fn audio_gate_allows_sustained_speech() {
    let mut stats = AudioStats::default();
    let mut carry = None;
    stats.ingest_bytes(&pcm_samples(0.08, 4800), &mut carry);
    assert!(stats.should_transcribe());
    assert!(stats.speech_duration().as_millis() >= 200);
}

#[test]
fn recording_command_uses_configured_source() {
    let cfg = AppConfig {
        audio_source: Some("alsa_input.usb".to_string()),
        whisper_threads: 8,
        ..AppConfig::default()
    };
    let command = build_recording_command(&cfg);
    assert_eq!(command.get_program().to_string_lossy(), "parec");
    assert_eq!(
        command_args(&command),
        vec![
            "--client-name=whisper-gtk",
            "--format=s16le",
            "--channels=1",
            "--rate=16000",
            "--latency-msec=30",
            "-d",
            "alsa_input.usb",
        ]
    );
}

#[test]
fn audio_source_selection_maps_default_to_none() {
    assert_eq!(audio_source_selection_to_config("Default source"), None);
    assert_eq!(
        audio_source_selection_to_config("alsa_input.usb"),
        Some("alsa_input.usb".to_string())
    );
}

#[test]
fn output_mode_selection_maps_dropdown_values() {
    assert_eq!(
        output_mode_selection_to_config("Direct typing"),
        Some(OutputMode::DirectTyping)
    );
    assert_eq!(
        output_mode_selection_to_config("Clipboard paste"),
        Some(OutputMode::ClipboardPaste)
    );
    assert_eq!(output_mode_selection_to_config("Other"), None);
}

#[test]
fn whisper_model_scanner_matches_packaged_layout() {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("whisper-gtk-model-scan-{stamp}"));
    let share = root.join("usr/share");
    let model_dir_a = share.join("whisper.cpp-model-base.en");
    let model_dir_b = share.join("whisper.cpp-model-large-v3-q5_0");
    fs::create_dir_all(&model_dir_a).unwrap();
    fs::create_dir_all(&model_dir_b).unwrap();
    fs::write(model_dir_a.join("ggml-base.en.bin"), b"a").unwrap();
    fs::write(model_dir_b.join("ggml-large-v3-q5_0.bin"), b"b").unwrap();
    fs::write(model_dir_b.join("not-a-model.txt"), b"c").unwrap();

    let models = list_whisper_models_in(&share);
    let expected = vec![
        model_dir_a.join("ggml-base.en.bin"),
        model_dir_b.join("ggml-large-v3-q5_0.bin"),
    ];
    assert_eq!(models, expected);

    let _ = fs::remove_dir_all(root);
}

fn pcm_samples(amplitude: f32, samples: usize) -> Vec<u8> {
    let sample = (amplitude.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
    let mut bytes = Vec::with_capacity(samples * 2);
    for _ in 0..samples {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[test]
fn raw_to_wav_creates_header() {
    let raw = std::env::temp_dir().join("whisper-gtk-test.raw");
    let wav = std::env::temp_dir().join("whisper-gtk-test.wav");
    let _ = fs::remove_file(&raw);
    let _ = fs::remove_file(&wav);
    fs::write(&raw, vec![0u8; 320]).unwrap();
    raw_to_wav(&raw, &wav).unwrap();
    let bytes = fs::read(&wav).unwrap();
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(&bytes[8..12], b"WAVE");
    let _ = fs::remove_file(raw);
    let _ = fs::remove_file(wav);
}
