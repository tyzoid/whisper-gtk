use crate::config::{AppConfig, OutputMode};
use gtk::gdk;
use gtk::prelude::DisplayExt;
use libxdo::XDo;
use psimple::Simple;
use pulse::callbacks::ListResult;
use pulse::context::{Context, FlagSet as ContextFlagSet, State};
use pulse::def::BufferAttr;
use pulse::mainloop::standard::Mainloop;
use pulse::sample::{Format, Spec};
use pulse::stream::Direction;
use std::ffi::CString;
use std::fs;
use std::io;
use std::os::raw::{c_char, c_int, c_long, c_uchar, c_uint, c_ulong, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState,
};

#[cfg(test)]
use std::fs::{File, OpenOptions};
#[cfg(test)]
use std::io::Write;
#[cfg(test)]
use std::os::unix::fs::OpenOptionsExt;

const SAMPLE_RATE: u32 = 16_000;
const BYTES_PER_SAMPLE: u64 = 2;
const RECORDING_LATENCY_MILLIS: u32 = 30;
const MIN_RECORDING_DURATION: Duration = Duration::from_millis(300);
const MIN_SPEECH_DURATION: Duration = Duration::from_millis(200);
const SPEECH_RMS_THRESHOLD: f32 = 0.015;
const WHISPER_MODEL_PREFIX: &str = "whisper.cpp-model-";
const WHISPER_MODEL_FILE_PREFIX: &str = "ggml-";
const WHISPER_MODEL_FILE_SUFFIX: &str = ".bin";
const DEFAULT_WHISPER_MODEL_PATH: &str = "/usr/share/whisper.cpp-model-base.en/ggml-base.en.bin";

pub fn default_whisper_model_path() -> PathBuf {
    let preferred = PathBuf::from(DEFAULT_WHISPER_MODEL_PATH);
    if preferred.is_file() {
        return preferred;
    }
    list_whisper_models()
        .into_iter()
        .next()
        .unwrap_or(preferred)
}

pub fn list_whisper_models() -> Vec<PathBuf> {
    list_whisper_models_in(Path::new("/usr/share"))
}

pub fn list_whisper_models_in(share_root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(share_root) else {
        return vec![];
    };
    let mut models = Vec::new();
    for entry in entries.flatten() {
        let model_dir = entry.path();
        let Some(dir_name) = model_dir.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !model_dir.is_dir() || !dir_name.starts_with(WHISPER_MODEL_PREFIX) {
            continue;
        }
        let Ok(files) = fs::read_dir(&model_dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if path.is_file()
                && file_name.starts_with(WHISPER_MODEL_FILE_PREFIX)
                && file_name.ends_with(WHISPER_MODEL_FILE_SUFFIX)
            {
                models.push(path);
            }
        }
    }
    models.sort();
    models
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub key: gdk::Key,
    pub mods: gdk::ModifierType,
}

impl Hotkey {
    pub fn parse(accelerator: &str) -> Option<Self> {
        gtk::accelerator_parse(accelerator).map(|(key, mods)| Self { key, mods })
    }

    pub fn display(&self) -> String {
        gtk::accelerator_get_label(self.key, self.mods).to_string()
    }
}

#[cfg(test)]
pub fn normalize_mods(mods: gdk::ModifierType) -> gdk::ModifierType {
    mods & (gdk::ModifierType::SHIFT_MASK
        | gdk::ModifierType::CONTROL_MASK
        | gdk::ModifierType::ALT_MASK
        | gdk::ModifierType::SUPER_MASK
        | gdk::ModifierType::HYPER_MASK
        | gdk::ModifierType::META_MASK)
}

#[cfg(test)]
pub fn hotkey_matches(candidate: Hotkey, key: gdk::Key, mods: gdk::ModifierType) -> bool {
    candidate.key == key && normalize_mods(candidate.mods) == normalize_mods(mods)
}

pub fn list_audio_sources() -> Vec<String> {
    with_pulse_context(|context, mainloop| {
        let introspector = context.introspect();
        let done = Arc::new(AtomicBool::new(false));
        let sources = Arc::new(Mutex::new(Vec::new()));
        let done_callback = Arc::clone(&done);
        let sources_callback = Arc::clone(&sources);
        let operation = introspector.get_source_info_list(move |result| match result {
            ListResult::Item(info) => {
                if let Some(name) = info.name.as_deref() {
                    sources_callback.lock().unwrap().push(name.to_string());
                }
            }
            ListResult::End => done_callback.store(true, Ordering::Release),
            ListResult::Error => {
                done_callback.store(true, Ordering::Release);
            }
        });

        wait_for_pulse_completion(mainloop, &done);
        drop(operation);

        let sources = sources.lock().unwrap().clone();
        Ok(sources)
    })
    .unwrap_or_default()
}

pub fn default_audio_source() -> Option<String> {
    with_pulse_context(|context, mainloop| {
        let introspector = context.introspect();
        let done = Arc::new(AtomicBool::new(false));
        let default_source = Arc::new(Mutex::new(None::<String>));
        let done_callback = Arc::clone(&done);
        let default_source_callback = Arc::clone(&default_source);
        let operation = introspector.get_server_info(move |info| {
            let source = info
                .default_source_name
                .as_deref()
                .map(|name| name.to_string());
            *default_source_callback.lock().unwrap() = source;
            done_callback.store(true, Ordering::Release);
        });

        wait_for_pulse_completion(mainloop, &done);
        drop(operation);

        let default_source = default_source.lock().unwrap().clone();
        Ok(default_source)
    })
    .unwrap_or(None)
}

pub struct RecordingSession {
    stop: Arc<AtomicBool>,
    worker: thread::JoinHandle<io::Result<CapturedRecording>>,
    level_rx: Receiver<f32>,
    started_at: Instant,
}

pub enum RecordingStop {
    Captured(Vec<f32>),
    Discarded(AudioStats),
}

impl RecordingSession {
    pub fn start(config: &AppConfig) -> io::Result<Self> {
        let spec = recording_sample_spec();
        let selected_device = config.audio_source.clone();
        let (level_tx, level_rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker = thread::spawn(move || {
            capture_recording_worker(spec, selected_device, worker_stop, level_tx)
        });
        Ok(Self {
            stop,
            worker,
            level_rx,
            started_at: Instant::now(),
        })
    }

    pub fn stop(self) -> io::Result<RecordingStop> {
        self.stop.store(true, Ordering::Release);
        let captured = self
            .worker
            .join()
            .map_err(|_| io::Error::other("recording worker thread panicked"))??;
        let stats = captured.stats;
        if !stats.should_transcribe() {
            return Ok(RecordingStop::Discarded(stats));
        }

        Ok(RecordingStop::Captured(captured.samples))
    }

    pub fn started_at(&self) -> Instant {
        self.started_at
    }

    pub fn try_read_level(&self) -> Option<f32> {
        self.level_rx.try_recv().ok()
    }
}

#[derive(Debug)]
struct CapturedRecording {
    samples: Vec<f32>,
    stats: AudioStats,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RecordingGeneration {
    current: Option<u64>,
    next: u64,
}

impl RecordingGeneration {
    pub fn next(&mut self) -> u64 {
        self.next = self.next.saturating_add(1);
        self.current = Some(self.next);
        self.next
    }

    pub fn current(&self) -> Option<u64> {
        self.current
    }

    pub fn stop_if_current(&mut self, generation: u64) -> bool {
        if self.current == Some(generation) {
            self.current = None;
            return true;
        }
        false
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct AudioStats {
    bytes: u64,
    speech_bytes: u64,
}

impl AudioStats {
    pub fn ingest_bytes(&mut self, bytes: &[u8], carry: &mut Option<u8>) -> AudioChunkMetrics {
        let (metrics, sample_count) = decode_pcm_s16le_chunk(bytes, carry, |_| {});
        self.ingest_sample_count(sample_count, metrics.rms);
        metrics
    }

    fn ingest_sample_count(&mut self, sample_count: u64, rms: f32) {
        let sample_bytes = sample_count * BYTES_PER_SAMPLE;
        self.bytes += sample_bytes;
        if rms >= SPEECH_RMS_THRESHOLD {
            self.speech_bytes += sample_bytes;
        }
    }

    pub fn duration(&self) -> Duration {
        duration_for_bytes(self.bytes)
    }

    pub fn speech_duration(&self) -> Duration {
        duration_for_bytes(self.speech_bytes)
    }

    pub fn should_transcribe(&self) -> bool {
        self.duration() >= MIN_RECORDING_DURATION && self.speech_duration() >= MIN_SPEECH_DURATION
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct AudioChunkMetrics {
    pub peak: f32,
    pub rms: f32,
}

pub fn append_recorded_s16le_chunk(
    bytes: &[u8],
    samples: &mut Vec<f32>,
    stats: &mut AudioStats,
    carry: &mut Option<u8>,
) -> AudioChunkMetrics {
    let (metrics, sample_count) = decode_pcm_s16le_chunk(bytes, carry, |sample| {
        samples.push(sample);
    });
    stats.ingest_sample_count(sample_count, metrics.rms);
    metrics
}

fn decode_pcm_s16le_chunk(
    bytes: &[u8],
    carry: &mut Option<u8>,
    mut push_sample: impl FnMut(f32),
) -> (AudioChunkMetrics, u64) {
    let mut peak = 0f32;
    let mut square_sum = 0f64;
    let mut samples = 0u64;
    let mut slice = bytes;

    if let (Some(previous), Some(current)) = (carry.take(), slice.first().copied()) {
        let sample = i16::from_le_bytes([previous, current]);
        let normalized = sample as f32 / i16::MAX as f32;
        peak = peak.max(normalized.abs());
        square_sum += (normalized as f64).powi(2);
        samples += 1;
        push_sample(normalized);
        slice = &slice[1..];
    }

    for chunk in slice.chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        let normalized = sample as f32 / i16::MAX as f32;
        peak = peak.max(normalized.abs());
        square_sum += (normalized as f64).powi(2);
        samples += 1;
        push_sample(normalized);
    }

    if let Some(last) = slice.chunks_exact(2).remainder().first().copied() {
        *carry = Some(last);
    }

    let rms = if samples == 0 {
        0.0
    } else {
        (square_sum / samples as f64).sqrt() as f32
    };
    (AudioChunkMetrics { peak, rms }, samples)
}

fn duration_for_bytes(bytes: u64) -> Duration {
    Duration::from_secs_f64(bytes as f64 / (SAMPLE_RATE as f64 * BYTES_PER_SAMPLE as f64))
}

#[cfg(test)]
pub fn raw_to_wav(raw_path: &Path, wav_path: &Path) -> io::Result<()> {
    let mut raw = File::open(raw_path)?;
    let raw_len = raw.metadata()?.len();
    let mut wav = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(wav_path)?;
    write_wav_header(&mut wav, raw_len as u32)?;
    io::copy(&mut raw, &mut wav)?;
    Ok(())
}

#[cfg(test)]
fn write_wav_header(writer: &mut File, data_len: u32) -> io::Result<()> {
    let byte_rate = SAMPLE_RATE * 2;
    let block_align = 2u16;
    let chunk_size = 36u32 + data_len;
    writer.write_all(b"RIFF")?;
    writer.write_all(&chunk_size.to_le_bytes())?;
    writer.write_all(b"WAVE")?;
    writer.write_all(b"fmt ")?;
    writer.write_all(&16u32.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?;
    writer.write_all(&1u16.to_le_bytes())?;
    writer.write_all(&SAMPLE_RATE.to_le_bytes())?;
    writer.write_all(&byte_rate.to_le_bytes())?;
    writer.write_all(&block_align.to_le_bytes())?;
    writer.write_all(&16u16.to_le_bytes())?;
    writer.write_all(b"data")?;
    writer.write_all(&data_len.to_le_bytes())?;
    Ok(())
}

fn whisper_model_path(configured: Option<&str>) -> PathBuf {
    if let Some(path) = configured {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("WHISPER_MODEL_PATH") {
        return PathBuf::from(path);
    }
    default_whisper_model_path()
}

fn load_whisper_context(model_path: &Path) -> io::Result<WhisperContext> {
    WhisperContext::new_with_params(
        model_path
            .to_str()
            .ok_or_else(|| io::Error::other("invalid whisper model path"))?,
        WhisperContextParameters::default(),
    )
    .map_err(|err| {
        io::Error::other(format!(
            "failed to load whisper model {}: {err}",
            model_path.display()
        ))
    })
}

pub fn preload_whisper_model(configured_model_path: Option<&str>) -> io::Result<WhisperContext> {
    let model_path = whisper_model_path(configured_model_path);
    load_whisper_context(&model_path)
}

pub fn preload_whisper_state(configured_model_path: Option<&str>) -> io::Result<WhisperState> {
    let ctx = preload_whisper_model(configured_model_path)?;
    ctx.create_state()
        .map_err(|err| io::Error::other(format!("failed to create whisper state: {err}")))
}

pub fn validate_whisper_model_path(model_path: &Path) -> io::Result<()> {
    let ctx = load_whisper_context(model_path)?;
    ctx.create_state()
        .map_err(|err| io::Error::other(format!("failed to create whisper state: {err}")))?;
    Ok(())
}

pub fn transcribe(
    samples: &[f32],
    configured_model_path: Option<&str>,
    n_threads: usize,
) -> io::Result<String> {
    let state = preload_whisper_state(configured_model_path)?;
    transcribe_with_state(state, samples, n_threads)
}

pub fn transcribe_with_state(
    mut state: WhisperState,
    samples: &[f32],
    n_threads: usize,
) -> io::Result<String> {
    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_n_threads(n_threads.max(1) as i32);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_special(false);
    params.set_translate(false);

    state
        .full(params, samples)
        .map_err(|err| io::Error::other(format!("whisper inference failed: {err}")))?;

    let mut text = String::new();
    let segments = state.full_n_segments();
    for idx in 0..segments {
        let segment = state
            .get_segment(idx)
            .ok_or_else(|| io::Error::other(format!("missing whisper segment at index {idx}")))?;
        let segment_text = segment
            .to_str()
            .map_err(|err| io::Error::other(format!("invalid utf-8 in whisper segment: {err}")))?;
        if !text.is_empty() {
            text.push(' ');
        }
        text.push_str(segment_text.trim());
    }

    Ok(text.trim().to_string())
}

pub fn type_text(text: &str) -> io::Result<()> {
    with_xdo(|xdo| {
        xdo.enter_text(text, 0)
            .map_err(|err| io::Error::other(format!("libxdo text injection failed: {err:?}")))
    })
    .or_else(|_| paste_text_from_clipboard(text))
}

pub fn paste_text_from_clipboard(text: &str) -> io::Result<()> {
    let display = gdk::Display::default()
        .ok_or_else(|| io::Error::other("no GTK display available for clipboard"))?;
    display.clipboard().set_text(text);
    display.primary_clipboard().set_text(text);

    with_xdo(|xdo| {
        xdo.send_keysequence("ctrl+v", 0)
            .map_err(|err| io::Error::other(format!("libxdo keysequence failed: {err:?}")))
    })
}

pub fn raise_and_move_window_by_title(title: &str, x: i32, y: i32) -> io::Result<()> {
    with_x11_display(|display| unsafe {
        let root = XDefaultRootWindow(display);
        let window = find_window_by_title(display, root, title)
            .ok_or_else(|| io::Error::other("failed to locate X11 window by title"))?;
        let _ = XRaiseWindow(display, window);
        let _ = XMoveWindow(display, window, x, y);
        x11_sync(display);
        Ok(())
    })
}

fn with_x11_display<T>(f: impl FnOnce(*mut XDisplay) -> io::Result<T>) -> io::Result<T> {
    unsafe {
        let display = XOpenDisplay(std::ptr::null());
        if display.is_null() {
            return Err(io::Error::other("failed to open X11 display"));
        }

        let result = f(display);
        let _ = XCloseDisplay(display);
        result
    }
}

fn x11_sync(display: *mut XDisplay) {
    unsafe {
        let _ = XSync(display, 0);
    }
}

fn with_xdo<T>(f: impl FnOnce(&XDo) -> io::Result<T>) -> io::Result<T> {
    let xdo = XDo::new(None)
        .map_err(|err| io::Error::other(format!("failed to create libxdo context: {err:?}")))?;
    f(&xdo)
}

pub fn run_output_mode(mode: OutputMode, text: &str) -> io::Result<()> {
    match mode {
        OutputMode::DirectTyping => type_text(text),
        OutputMode::ClipboardPaste => paste_text_from_clipboard(text),
    }
}

unsafe fn find_window_by_title(
    display: *mut XDisplay,
    window: c_ulong,
    title: &str,
) -> Option<c_ulong> {
    if window_title(display, window).as_deref() == Some(title) {
        return Some(window);
    }

    let mut root_return = 0;
    let mut parent_return = 0;
    let mut children_return: *mut c_ulong = std::ptr::null_mut();
    let mut child_count = 0u32;
    if XQueryTree(
        display,
        window,
        &mut root_return,
        &mut parent_return,
        &mut children_return,
        &mut child_count,
    ) == 0
    {
        return None;
    }

    let mut found = None;
    if !children_return.is_null() {
        for idx in 0..child_count as usize {
            let child = *children_return.add(idx);
            found = find_window_by_title(display, child, title);
            if found.is_some() {
                break;
            }
        }
        let _ = XFree(children_return as *mut c_void);
    }

    found
}

unsafe fn window_title(display: *mut XDisplay, window: c_ulong) -> Option<String> {
    let mut window_name: *mut c_char = std::ptr::null_mut();
    if XFetchName(display, window, &mut window_name) == 0 || window_name.is_null() {
        return None;
    }
    let title = std::ffi::CStr::from_ptr(window_name)
        .to_str()
        .ok()
        .map(|title| title.to_string());
    let _ = XFree(window_name as *mut c_void);
    title
}

fn recording_sample_spec() -> Spec {
    let spec = Spec {
        format: Format::S16le,
        channels: 1,
        rate: SAMPLE_RATE,
    };
    assert!(spec.is_valid(), "invalid PulseAudio sample spec");
    spec
}

fn recording_chunk_bytes() -> usize {
    ((SAMPLE_RATE as usize * RECORDING_LATENCY_MILLIS as usize) / 1000) * 2
}

fn recording_buffer_attr() -> BufferAttr {
    BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: recording_chunk_bytes() as u32,
    }
}

fn capture_recording_worker(
    spec: Spec,
    selected_device: Option<String>,
    stop: Arc<AtomicBool>,
    level_tx: Sender<f32>,
) -> io::Result<CapturedRecording> {
    let buffer_attr = recording_buffer_attr();
    let simple = Simple::new(
        None,
        "whisper-gtk",
        Direction::Record,
        selected_device.as_deref(),
        "whisper-gtk",
        &spec,
        None,
        Some(&buffer_attr),
    )
    .map_err(|err| io::Error::other(format!("failed to open PulseAudio capture stream: {err}")))?;

    let mut samples = Vec::new();
    let mut stats = AudioStats::default();
    let mut carry = None;
    let mut buffer = vec![0u8; recording_chunk_bytes()];

    while !stop.load(Ordering::Acquire) {
        simple
            .read(&mut buffer)
            .map_err(|err| io::Error::other(format!("PulseAudio capture failed: {err}")))?;
        let metrics = append_recorded_s16le_chunk(&buffer, &mut samples, &mut stats, &mut carry);
        let _ = level_tx.send(metrics.peak.clamp(0.0, 1.0));
    }

    Ok(CapturedRecording { samples, stats })
}

fn with_pulse_context<T>(
    f: impl FnOnce(&mut Context, &mut Mainloop) -> io::Result<T>,
) -> io::Result<T> {
    let mut mainloop =
        Mainloop::new().ok_or_else(|| io::Error::other("failed to create PulseAudio mainloop"))?;
    let mut context = Context::new(&mainloop, "whisper-gtk")
        .ok_or_else(|| io::Error::other("failed to create PulseAudio context"))?;
    context
        .connect(None, ContextFlagSet::NOFLAGS, None)
        .map_err(|err| io::Error::other(format!("failed to connect to PulseAudio: {err}")))?;

    while !matches!(
        context.get_state(),
        State::Ready | State::Failed | State::Terminated
    ) {
        let _ = mainloop.iterate(true);
    }

    if !matches!(context.get_state(), State::Ready) {
        return Err(io::Error::other(format!(
            "PulseAudio context initialization failed: {:?}",
            context.get_state()
        )));
    }

    f(&mut context, &mut mainloop)
}

fn wait_for_pulse_completion(mainloop: &mut Mainloop, done: &Arc<AtomicBool>) {
    while !done.load(Ordering::Acquire) {
        let _ = mainloop.iterate(true);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorGeometry {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

pub fn overlay_position_for_monitor(
    monitor: MonitorGeometry,
    overlay_width: i32,
    overlay_height: i32,
) -> (i32, i32) {
    let x = monitor.x + (monitor.width - overlay_width).div_euclid(2);
    let center_y = monitor.y + monitor.height - ((monitor.height as f32) * 0.15).round() as i32;
    let y = center_y - overlay_height.div_euclid(2);
    (x.max(monitor.x), y.max(monitor.y))
}

pub fn focused_monitor_geometry() -> Option<MonitorGeometry> {
    unsafe {
        let display = XOpenDisplay(std::ptr::null());
        if display.is_null() {
            return None;
        }

        let root = XDefaultRootWindow(display);
        let active_window = active_window_for_display(display, root).unwrap_or(root);
        let monitor = monitor_for_window(display, root, active_window)
            .or_else(|| primary_monitor_for_root(display, root));

        let _ = XCloseDisplay(display);
        monitor
    }
}

unsafe fn active_window_for_display(display: *mut XDisplay, root: c_ulong) -> Option<c_ulong> {
    let active_atom = XInternAtom(
        display,
        active_window_atom_name().as_ptr() as *const c_char,
        0,
    );
    if active_atom != 0 {
        let mut actual_type: c_ulong = 0;
        let mut actual_format: c_int = 0;
        let mut nitems: c_ulong = 0;
        let mut bytes_after: c_ulong = 0;
        let mut data: *mut c_uchar = std::ptr::null_mut();
        let status = XGetWindowProperty(
            display,
            root,
            active_atom,
            0,
            1,
            0,
            ANY_PROPERTY_TYPE,
            &mut actual_type,
            &mut actual_format,
            &mut nitems,
            &mut bytes_after,
            &mut data,
        );
        if status == 0 && !data.is_null() && actual_format == 32 && nitems > 0 {
            let window = *(data as *const c_ulong);
            let _ = XFree(data as *mut c_void);
            if window != 0 {
                return Some(window);
            }
        } else if !data.is_null() {
            let _ = XFree(data as *mut c_void);
        }
    }

    let mut focused: c_ulong = 0;
    let mut revert_to: c_int = 0;
    if XGetInputFocus(display, &mut focused, &mut revert_to) == 1 && focused != 0 {
        Some(focused)
    } else {
        None
    }
}

unsafe fn monitor_for_window(
    display: *mut XDisplay,
    root: c_ulong,
    window: c_ulong,
) -> Option<MonitorGeometry> {
    let (x, y, width, height) = window_bounds(display, root, window)?;
    let center_x = x + width / 2;
    let center_y = y + height / 2;
    let mut count = 0;
    let monitors = XRRGetMonitors(display, root, 1, &mut count);
    if monitors.is_null() || count <= 0 {
        return Some(MonitorGeometry {
            x: 0,
            y: 0,
            width: XDisplayWidth(display, 0),
            height: XDisplayHeight(display, 0),
        });
    }

    let mut chosen = None;
    for idx in 0..count {
        let monitor = *monitors.add(idx as usize);
        let rect = MonitorGeometry {
            x: monitor.x,
            y: monitor.y,
            width: monitor.width,
            height: monitor.height,
        };
        if center_x >= rect.x
            && center_x < rect.x + rect.width
            && center_y >= rect.y
            && center_y < rect.y + rect.height
        {
            chosen = Some(rect);
            break;
        }
    }

    if chosen.is_none() {
        chosen = Some(MonitorGeometry {
            x: (*monitors).x,
            y: (*monitors).y,
            width: (*monitors).width,
            height: (*monitors).height,
        });
    }

    XRRFreeMonitors(monitors);
    chosen
}

unsafe fn primary_monitor_for_root(
    display: *mut XDisplay,
    root: c_ulong,
) -> Option<MonitorGeometry> {
    let mut count = 0;
    let monitors = XRRGetMonitors(display, root, 1, &mut count);
    if monitors.is_null() || count <= 0 {
        return Some(MonitorGeometry {
            x: 0,
            y: 0,
            width: XDisplayWidth(display, 0),
            height: XDisplayHeight(display, 0),
        });
    }

    let mut chosen = None;
    for idx in 0..count {
        let monitor = *monitors.add(idx as usize);
        if monitor.primary != 0 {
            chosen = Some(MonitorGeometry {
                x: monitor.x,
                y: monitor.y,
                width: monitor.width,
                height: monitor.height,
            });
            break;
        }
    }

    if chosen.is_none() {
        let monitor = *monitors;
        chosen = Some(MonitorGeometry {
            x: monitor.x,
            y: monitor.y,
            width: monitor.width,
            height: monitor.height,
        });
    }

    XRRFreeMonitors(monitors);
    chosen
}

unsafe fn window_bounds(
    display: *mut XDisplay,
    root: c_ulong,
    window: c_ulong,
) -> Option<(i32, i32, i32, i32)> {
    let mut root_ret: c_ulong = 0;
    let mut x = 0;
    let mut y = 0;
    let mut width: c_uint = 0;
    let mut height: c_uint = 0;
    let mut border: c_uint = 0;
    let mut depth: c_uint = 0;
    if XGetGeometry(
        display,
        window,
        &mut root_ret,
        &mut x,
        &mut y,
        &mut width,
        &mut height,
        &mut border,
        &mut depth,
    ) == 0
    {
        return None;
    }

    let mut child: c_ulong = 0;
    let mut root_x = 0;
    let mut root_y = 0;
    if XTranslateCoordinates(
        display,
        window,
        root,
        0,
        0,
        &mut root_x,
        &mut root_y,
        &mut child,
    ) == 0
    {
        return None;
    }

    Some((root_x, root_y, width as i32, height as i32))
}

fn active_window_atom_name() -> &'static [u8] {
    b"_NET_ACTIVE_WINDOW\0"
}

#[repr(C)]
struct XDisplay(c_void);

#[repr(C)]
#[derive(Clone, Copy)]
struct XKeyEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    window: c_ulong,
    root: c_ulong,
    subwindow: c_ulong,
    time: c_ulong,
    x: c_int,
    y: c_int,
    x_root: c_int,
    y_root: c_int,
    state: u32,
    keycode: u32,
    same_screen: c_int,
}

#[repr(C)]
union XEvent {
    type_: c_int,
    key: XKeyEvent,
    pad: [c_long; 24],
}

const KEY_PRESS: c_int = 2;
const KEY_RELEASE: c_int = 3;
const GRAB_MODE_ASYNC: c_int = 1;
const MOD2_MASK: u32 = 1 << 4;
const ANY_PROPERTY_TYPE: c_ulong = 0;
const SHIFT_MASK: c_uint = 1;
const CONTROL_MASK: c_uint = 1 << 2;
const MOD1_MASK: c_uint = 1 << 3;
const MOD4_MASK: c_uint = 1 << 6;

#[link(name = "X11")]
extern "C" {
    fn XOpenDisplay(display_name: *const c_char) -> *mut XDisplay;
    fn XCloseDisplay(display: *mut XDisplay) -> c_int;
    fn XDefaultRootWindow(display: *mut XDisplay) -> c_ulong;
    fn XNextEvent(display: *mut XDisplay, event_return: *mut XEvent) -> c_int;
    fn XPending(display: *mut XDisplay) -> c_int;
    fn XInternAtom(
        display: *mut XDisplay,
        atom_name: *const c_char,
        only_if_exists: c_int,
    ) -> c_ulong;
    fn XGetWindowProperty(
        display: *mut XDisplay,
        w: c_ulong,
        property: c_ulong,
        long_offset: c_long,
        long_length: c_long,
        delete: c_int,
        req_type: c_ulong,
        actual_type_return: *mut c_ulong,
        actual_format_return: *mut c_int,
        nitems_return: *mut c_ulong,
        bytes_after_return: *mut c_ulong,
        prop_return: *mut *mut c_uchar,
    ) -> c_int;
    fn XFree(data: *mut c_void) -> c_int;
    fn XGetInputFocus(
        display: *mut XDisplay,
        focus_return: *mut c_ulong,
        revert_to_return: *mut c_int,
    ) -> c_int;
    fn XGetGeometry(
        display: *mut XDisplay,
        d: c_ulong,
        root_return: *mut c_ulong,
        x_return: *mut c_int,
        y_return: *mut c_int,
        width_return: *mut c_uint,
        height_return: *mut c_uint,
        border_width_return: *mut c_uint,
        depth_return: *mut c_uint,
    ) -> c_int;
    fn XTranslateCoordinates(
        display: *mut XDisplay,
        src_w: c_ulong,
        dest_w: c_ulong,
        src_x: c_int,
        src_y: c_int,
        dest_x_return: *mut c_int,
        dest_y_return: *mut c_int,
        child_return: *mut c_ulong,
    ) -> c_int;
    fn XkbSetDetectableAutoRepeat(
        display: *mut XDisplay,
        detectable: c_int,
        supported_rtn: *mut c_int,
    ) -> c_int;
    fn XkbKeycodeToKeysym(
        display: *mut XDisplay,
        keycode: u8,
        group: c_int,
        level: c_int,
    ) -> c_ulong;
    fn XStringToKeysym(string: *const c_char) -> c_ulong;
    fn XKeysymToKeycode(display: *mut XDisplay, keysym: c_ulong) -> u8;
    fn XQueryKeymap(display: *mut XDisplay, keys_return: *mut c_char) -> c_int;
    fn XQueryTree(
        display: *mut XDisplay,
        window: c_ulong,
        root_return: *mut c_ulong,
        parent_return: *mut c_ulong,
        children_return: *mut *mut c_ulong,
        nchildren_return: *mut c_uint,
    ) -> c_int;
    fn XFetchName(
        display: *mut XDisplay,
        window: c_ulong,
        window_name_return: *mut *mut c_char,
    ) -> c_int;
    fn XGrabKey(
        display: *mut XDisplay,
        keycode: c_int,
        modifiers: c_uint,
        grab_window: c_ulong,
        owner_events: c_int,
        pointer_mode: c_int,
        keyboard_mode: c_int,
    ) -> c_int;
    fn XUngrabKey(
        display: *mut XDisplay,
        keycode: c_int,
        modifiers: c_uint,
        grab_window: c_ulong,
    ) -> c_int;
    fn XSync(display: *mut XDisplay, discard: c_int) -> c_int;
    fn XMoveWindow(display: *mut XDisplay, window: c_ulong, x: c_int, y: c_int) -> c_int;
    fn XRaiseWindow(display: *mut XDisplay, window: c_ulong) -> c_int;
    fn XDisplayWidth(display: *mut XDisplay, screen_number: c_int) -> c_int;
    fn XDisplayHeight(display: *mut XDisplay, screen_number: c_int) -> c_int;
}

#[link(name = "Xrandr")]
extern "C" {
    fn XRRGetMonitors(
        display: *mut XDisplay,
        window: c_ulong,
        get_active: c_int,
        nmonitors: *mut c_int,
    ) -> *mut XRRMonitorInfo;
    fn XRRFreeMonitors(monitors: *mut XRRMonitorInfo);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XRRMonitorInfo {
    name: c_ulong,
    primary: c_int,
    automatic: c_int,
    noutput: c_int,
    x: c_int,
    y: c_int,
    width: c_int,
    height: c_int,
    mwidth: c_int,
    mheight: c_int,
    outputs: *mut c_ulong,
}

fn normalize_x11_mods(mods: c_uint) -> c_uint {
    mods & (SHIFT_MASK | CONTROL_MASK | MOD1_MASK | MOD4_MASK)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct X11Hotkey {
    keysym: c_ulong,
    modifiers: c_uint,
}

pub fn parse_x11_hotkey(accelerator: &str) -> Option<X11Hotkey> {
    let accelerator = accelerator.trim();
    if accelerator.is_empty() {
        return None;
    }

    let mut modifiers = 0;
    let mut key = None;
    let mut rest = accelerator;

    while let Some(stripped) = rest.strip_prefix('<') {
        let Some(end) = stripped.find('>') else {
            break;
        };
        let token = &stripped[..end];
        rest = stripped[end + 1..].trim_start();
        if let Some(mask) = x11_modifier_token(token) {
            modifiers |= mask;
        } else if rest.is_empty() {
            key = Some(token);
        } else {
            return None;
        }
    }

    if key.is_none() && !rest.is_empty() {
        if rest.contains('+') {
            for token in rest
                .split('+')
                .map(str::trim)
                .filter(|token| !token.is_empty())
            {
                if let Some(mask) = x11_modifier_token(token) {
                    modifiers |= mask;
                } else {
                    key = Some(token);
                }
            }
        } else {
            key = Some(rest);
        }
    }

    let key = key?;
    let key = CString::new(key).ok()?;
    let keysym = unsafe { XStringToKeysym(key.as_ptr()) };
    (keysym != 0).then_some(X11Hotkey { keysym, modifiers })
}

fn x11_modifier_token(token: &str) -> Option<c_uint> {
    match token.to_ascii_lowercase().as_str() {
        "shift" => Some(SHIFT_MASK),
        "control" | "ctrl" | "primary" => Some(CONTROL_MASK),
        "alt" | "mod1" => Some(MOD1_MASK),
        "super" | "meta" | "hyper" | "mod4" => Some(MOD4_MASK),
        _ => None,
    }
}

unsafe fn hotkey_is_physically_down(
    display: *mut XDisplay,
    keymap: &[c_char; 32],
    grab: ActiveGrab,
) -> bool {
    keycode_is_down(keymap, grab.keycode as u8)
        && required_modifiers_are_down(display, keymap, grab.modifiers)
}

pub fn keycode_is_down(keymap: &[c_char; 32], keycode: u8) -> bool {
    let index = (keycode / 8) as usize;
    let bit = keycode % 8;
    keymap
        .get(index)
        .map(|byte| (*byte as u8 & (1 << bit)) != 0)
        .unwrap_or(false)
}

unsafe fn required_modifiers_are_down(
    display: *mut XDisplay,
    keymap: &[c_char; 32],
    modifiers: c_uint,
) -> bool {
    (modifiers & SHIFT_MASK == 0 || any_keysym_is_down(display, keymap, &["Shift_L", "Shift_R"]))
        && (modifiers & CONTROL_MASK == 0
            || any_keysym_is_down(display, keymap, &["Control_L", "Control_R"]))
        && (modifiers & MOD1_MASK == 0
            || any_keysym_is_down(display, keymap, &["Alt_L", "Alt_R", "Meta_L", "Meta_R"]))
        && (modifiers & MOD4_MASK == 0
            || any_keysym_is_down(
                display,
                keymap,
                &[
                    "Super_L", "Super_R", "Hyper_L", "Hyper_R", "Meta_L", "Meta_R",
                ],
            ))
}

unsafe fn any_keysym_is_down(
    display: *mut XDisplay,
    keymap: &[c_char; 32],
    names: &[&str],
) -> bool {
    names.iter().any(|name| {
        let Ok(name) = CString::new(*name) else {
            return false;
        };
        let keysym = XStringToKeysym(name.as_ptr());
        keysym != 0 && keycode_is_down(keymap, XKeysymToKeycode(display, keysym))
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveGrab {
    keycode: c_int,
    modifiers: c_uint,
}

unsafe fn install_grab(
    display: *mut XDisplay,
    root: c_ulong,
    hotkey: X11Hotkey,
) -> Option<ActiveGrab> {
    let keycode = XKeysymToKeycode(display, hotkey.keysym) as c_int;
    if keycode == 0 {
        return None;
    }
    let modifiers = hotkey.modifiers;
    for extra in [
        0,
        gdk::ModifierType::LOCK_MASK.bits(),
        MOD2_MASK,
        gdk::ModifierType::LOCK_MASK.bits() | MOD2_MASK,
    ] {
        let _ = XGrabKey(
            display,
            keycode,
            modifiers | extra,
            root,
            0,
            GRAB_MODE_ASYNC,
            GRAB_MODE_ASYNC,
        );
    }
    let _ = XSync(display, 0);
    Some(ActiveGrab { keycode, modifiers })
}

unsafe fn uninstall_grab(display: *mut XDisplay, root: c_ulong, grab: ActiveGrab) {
    for extra in [
        0,
        gdk::ModifierType::LOCK_MASK.bits(),
        MOD2_MASK,
        gdk::ModifierType::LOCK_MASK.bits() | MOD2_MASK,
    ] {
        let _ = XUngrabKey(display, grab.keycode, grab.modifiers | extra, root);
    }
    let _ = XSync(display, 0);
}

pub fn spawn_xev_hotkey_listener(config: Arc<Mutex<AppConfig>>, sender: Sender<crate::AppEvent>) {
    thread::spawn(move || unsafe {
        let display = XOpenDisplay(std::ptr::null());
        if display.is_null() {
            let _ = sender.send(crate::AppEvent::Error(
                "Failed to open X11 display for hotkey listener".to_string(),
            ));
            return;
        }

        let root = XDefaultRootWindow(display);
        let mut supported = 0;
        let _ = XkbSetDetectableAutoRepeat(display, 1, &mut supported);
        let mut active_hotkey: Option<X11Hotkey> = None;
        let mut active_grab: Option<ActiveGrab> = None;
        let mut hotkey_pressed = false;

        loop {
            let configured_hotkey = config
                .lock()
                .ok()
                .and_then(|guard| parse_x11_hotkey(&guard.hotkey));
            if configured_hotkey != active_hotkey {
                if hotkey_pressed {
                    let _ = sender.send(crate::AppEvent::HotkeyReleased);
                    hotkey_pressed = false;
                }
                if let Some(grab) = active_grab.take() {
                    uninstall_grab(display, root, grab);
                }
                active_hotkey = configured_hotkey;
                active_grab = active_hotkey.and_then(|hotkey| install_grab(display, root, hotkey));
            }

            if XPending(display) == 0 {
                if hotkey_pressed {
                    if let Some(grab) = active_grab {
                        let mut keymap = [0 as c_char; 32];
                        if XQueryKeymap(display, keymap.as_mut_ptr()) != 0
                            && !hotkey_is_physically_down(display, &keymap, grab)
                        {
                            let _ = sender.send(crate::AppEvent::HotkeyReleased);
                            hotkey_pressed = false;
                        }
                    }
                }
                thread::sleep(std::time::Duration::from_millis(50));
                continue;
            }

            let mut event = std::mem::MaybeUninit::<XEvent>::zeroed();
            let _ = XNextEvent(display, event.as_mut_ptr());
            let event = event.assume_init();
            let key = event.key;

            match key.type_ {
                KEY_PRESS | KEY_RELEASE => {
                    let Some(hotkey) = active_hotkey else {
                        continue;
                    };

                    let keysym = XkbKeycodeToKeysym(display, key.keycode as u8, 0, 0);

                    if keysym == hotkey.keysym && normalize_x11_mods(key.state) == hotkey.modifiers
                    {
                        match key.type_ {
                            KEY_PRESS if !hotkey_pressed => {
                                let _ = sender.send(crate::AppEvent::HotkeyPressed);
                                hotkey_pressed = true;
                            }
                            KEY_RELEASE if hotkey_pressed => {
                                let _ = sender.send(crate::AppEvent::HotkeyReleased);
                                hotkey_pressed = false;
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    });
}
