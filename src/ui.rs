use crate::config::{AppConfig, OutputMode};
use crate::services::{default_whisper_model_dir, list_audio_sources, list_whisper_models, Hotkey};
use gtk::gdk;
use gtk::prelude::*;
use gtk::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, ComboBoxText, CssProvider,
    DrawingArea, FileChooserAction, FileChooserNative, Label, Orientation, ResponseType,
    SpinButton, STYLE_PROVIDER_PRIORITY_APPLICATION,
};
use std::cell::RefCell;
use std::f64::consts::TAU;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

pub struct OverlayUi {
    pub window: ApplicationWindow,
    pub meter: OverlayMeter,
}

#[derive(Clone)]
pub struct OverlayMeter {
    area: DrawingArea,
    state: Rc<RefCell<WaveformState>>,
}

#[derive(Debug, Clone)]
pub struct WaveformState {
    level: f32,
    phase: f32,
}

impl Default for WaveformState {
    fn default() -> Self {
        Self {
            level: 0.25,
            phase: 0.0,
        }
    }
}

impl WaveformState {
    pub fn set_level(&mut self, level: f32) {
        self.level = level.clamp(0.0, 1.0);
    }

    pub fn advance(&mut self) {
        self.phase = (self.phase + 0.08) % 1.0;
    }

    pub fn bar_heights(&self, bar_count: usize) -> Vec<f64> {
        let mut bars = Vec::with_capacity(bar_count);
        let level = self.level as f64;
        let phase = self.phase as f64 * TAU;
        for index in 0..bar_count {
            let wave = ((phase + index as f64 * 0.78).sin() * 0.5 + 0.5).powf(0.85);
            let height = 0.16 + level * 0.72 + wave * 0.28;
            bars.push(height.clamp(0.08, 1.0));
        }
        bars
    }
}

impl OverlayMeter {
    pub fn set_level(&self, level: f32) {
        if let Ok(mut state) = self.state.try_borrow_mut() {
            state.set_level(level);
        }
        self.area.queue_draw();
    }

    pub fn tick(&self) {
        if let Ok(mut state) = self.state.try_borrow_mut() {
            state.advance();
        }
        self.area.queue_draw();
    }
}

pub fn build_overlay(app: &Application) -> OverlayUi {
    install_overlay_css();
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Whisper Recording")
        .decorated(false)
        .resizable(false)
        .focusable(false)
        .modal(false)
        .default_width(200)
        .default_height(40)
        .build();
    window.set_hide_on_close(true);
    window.add_css_class("whisper-recording-overlay");
    let state = Rc::new(RefCell::new(WaveformState::default()));
    let area = DrawingArea::new();
    area.add_css_class("whisper-recording-overlay");
    area.set_content_width(200);
    area.set_content_height(40);
    area.set_hexpand(false);
    area.set_vexpand(false);

    let draw_state = state.clone();
    area.set_draw_func(move |_, cr, width, height| {
        draw_overlay(cr, width, height, &draw_state.borrow());
    });

    window.set_child(Some(&area));

    OverlayUi {
        window,
        meter: OverlayMeter { area, state },
    }
}

fn install_overlay_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };
    let provider = CssProvider::new();
    provider.load_from_data(
        r#"
        window.whisper-recording-overlay,
        .whisper-recording-overlay {
            background: transparent;
        }
        "#,
    );
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn draw_overlay(cr: &gtk::cairo::Context, width: i32, height: i32, state: &WaveformState) {
    let width = width as f64;
    let height = height as f64;
    let radius = height / 2.0;

    cr.set_source_rgba(0.07, 0.09, 0.11, 0.9);
    rounded_rect(cr, 0.5, 0.5, width - 1.0, height - 1.0, radius - 0.5);
    let _ = cr.fill();

    cr.set_source_rgba(0.18, 0.78, 0.55, 0.22);
    cr.set_line_width(1.0);
    rounded_rect(cr, 0.5, 0.5, width - 1.0, height - 1.0, radius - 0.5);
    let _ = cr.stroke();

    draw_mic_icon(cr, 16.0, 10.0, 18.0, 20.0);
    draw_waveform(cr, state, 46.0, 8.0, width - 56.0, height - 16.0);
}

fn draw_mic_icon(cr: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    let center_x = x + width / 2.0;
    let top = y + height * 0.16;
    let body_height = height * 0.5;
    let body_width = width * 0.34;

    cr.set_source_rgba(0.93, 0.96, 0.97, 0.94);
    rounded_rect(
        cr,
        center_x - body_width / 2.0,
        top,
        body_width,
        body_height,
        body_width / 2.0,
    );
    let _ = cr.fill();

    cr.set_line_width(1.6);
    cr.move_to(center_x, top + body_height);
    cr.line_to(center_x, top + height * 0.82);
    let _ = cr.stroke();

    cr.set_line_width(1.4);
    cr.move_to(center_x - width * 0.18, top + height * 0.82);
    cr.line_to(center_x + width * 0.18, top + height * 0.82);
    let _ = cr.stroke();

    cr.set_line_width(1.2);
    cr.arc(
        center_x,
        top + body_height * 0.55,
        body_width * 0.52,
        0.0,
        TAU,
    );
    let _ = cr.stroke();
}

fn draw_waveform(
    cr: &gtk::cairo::Context,
    state: &WaveformState,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) {
    let bars = 11usize;
    let gaps = 3.0f64;
    let bar_width = ((width - gaps * (bars as f64 - 1.0)) / bars as f64).max(2.0);
    let available_height = height.max(1.0);
    let bar_heights = state.bar_heights(bars);
    let color = if state.level > 0.5 {
        (0.35, 0.95, 0.63, 0.96)
    } else {
        (0.50, 0.90, 0.72, 0.90)
    };
    cr.set_source_rgba(color.0, color.1, color.2, color.3);

    for (index, factor) in bar_heights.into_iter().enumerate() {
        let bar_height = available_height * factor;
        let left = x + index as f64 * (bar_width + gaps);
        let top = y + (available_height - bar_height) / 2.0;
        rounded_rect(
            cr,
            left,
            top,
            bar_width,
            bar_height,
            (bar_width / 2.0).min(3.0),
        );
        let _ = cr.fill();
    }
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    cr.new_sub_path();
    cr.arc(x + width - radius, y + radius, radius, -TAU / 4.0, 0.0);
    cr.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        TAU / 4.0,
    );
    cr.arc(
        x + radius,
        y + height - radius,
        radius,
        TAU / 4.0,
        TAU / 2.0,
    );
    cr.arc(x + radius, y + radius, radius, TAU / 2.0, TAU * 0.75);
    cr.close_path();
}

pub fn build_settings_window(
    app: &Application,
    config: Arc<Mutex<AppConfig>>,
) -> ApplicationWindow {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Whisper Settings")
        .default_width(460)
        .default_height(320)
        .resizable(false)
        .build();
    window.set_hide_on_close(true);

    let root = GtkBox::new(Orientation::Vertical, 12);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);

    let hotkey_heading = Label::new(Some("Hotkey"));
    hotkey_heading.set_halign(Align::Start);
    let hotkey_button = Button::new();
    let capture_hint = Label::new(Some(
        "Click the button, then press the key combo to record.",
    ));
    capture_hint.set_halign(Align::Start);
    let hotkey_value = Label::new(None);
    hotkey_value.set_halign(Align::Start);

    let audio_heading = Label::new(Some("Audio Source"));
    audio_heading.set_halign(Align::Start);
    let audio_combo = ComboBoxText::new();

    let mode_heading = Label::new(Some("Output Mode"));
    mode_heading.set_halign(Align::Start);
    let mode_combo = ComboBoxText::new();
    mode_combo.append(Some("typing"), "Direct typing");
    mode_combo.append(Some("clipboard"), "Clipboard paste");

    let duration_heading = Label::new(Some("Max Recording Duration"));
    duration_heading.set_halign(Align::Start);
    let duration_spin = SpinButton::with_range(5.0, 180.0, 1.0);
    let model_heading = Label::new(Some("Whisper Model"));
    model_heading.set_halign(Align::Start);
    let model_combo = ComboBoxText::new();

    let refresh_sources = |audio_combo: &ComboBoxText, config: &AppConfig| {
        audio_combo.remove_all();
        audio_combo.append(Some("default"), "Default source");
        for source in list_audio_sources() {
            audio_combo.append(Some(&source), &source);
        }
        let active = config
            .audio_source
            .clone()
            .unwrap_or_else(|| "default".to_string());
        audio_combo.set_active_id(Some(&active));
        if audio_combo.active_id().is_none() {
            audio_combo.set_active_id(Some("default"));
        }
    };

    {
        let cfg = config.lock().unwrap().clone();
        hotkey_value.set_label(
            &Hotkey::parse(&cfg.hotkey)
                .map(|h| h.display())
                .unwrap_or_else(|| cfg.hotkey.clone()),
        );
        hotkey_button.set_label(&cfg.hotkey);
        refresh_sources(&audio_combo, &cfg);
        mode_combo.set_active_id(Some(match cfg.output_mode {
            OutputMode::DirectTyping => "typing",
            OutputMode::ClipboardPaste => "clipboard",
        }));
        duration_spin.set_value(cfg.max_recording_secs as f64);

        model_combo.remove_all();
        for model_path in list_whisper_models() {
            let id = model_path.to_string_lossy().to_string();
            let label = model_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(&id)
                .to_string();
            model_combo.append(Some(&id), &label);
        }
        model_combo.append(Some("other"), "Other...");
        if let Some(path) = cfg.model_path.as_deref() {
            model_combo.set_active_id(Some(path));
            if model_combo.active_id().is_none() {
                model_combo.set_active_id(Some("other"));
            }
        } else {
            let default = default_whisper_model_dir().join("ggml-base.en.bin");
            let default_id = default.to_string_lossy().to_string();
            model_combo.set_active_id(Some(&default_id));
            if model_combo.active_id().is_none() {
                model_combo.set_active(0);
            }
        }
    }

    let capture_mode = std::rc::Rc::new(std::cell::RefCell::new(false));
    let capture_mode_button = capture_mode.clone();
    let hotkey_button_for_button = hotkey_button.clone();
    hotkey_button.connect_clicked(move |_| {
        *capture_mode_button.borrow_mut() = true;
        hotkey_button_for_button.set_label("Press keys...");
    });

    let controller = gtk::EventControllerKey::new();
    let capture_mode_for_keys = capture_mode.clone();
    let config_for_keys = config.clone();
    let hotkey_button_for_keys = hotkey_button.clone();
    let hotkey_value_for_keys = hotkey_value.clone();
    controller.connect_key_pressed(move |_, key, _keycode, state| {
        if !*capture_mode_for_keys.borrow() {
            return gtk::glib::Propagation::Proceed;
        }
        if key == gdk::Key::Escape {
            *capture_mode_for_keys.borrow_mut() = false;
            if let Some(cfg) = Hotkey::parse(&config_for_keys.lock().unwrap().hotkey) {
                hotkey_button_for_keys.set_label(&cfg.display());
                hotkey_value_for_keys.set_label(&cfg.display());
            }
            return gtk::glib::Propagation::Stop;
        }

        let mods = state & gtk::accelerator_get_default_mod_mask();
        if gtk::accelerator_valid(key, mods) {
            let accel = gtk::accelerator_name(key, mods).to_string();
            {
                let mut cfg = config_for_keys.lock().unwrap();
                cfg.hotkey = accel.clone();
                let _ = cfg.save();
            }
            *capture_mode_for_keys.borrow_mut() = false;
            hotkey_button_for_keys.set_label(&accel);
            hotkey_value_for_keys.set_label(&accel);
            return gtk::glib::Propagation::Stop;
        }

        gtk::glib::Propagation::Proceed
    });
    window.add_controller(controller);

    let config_for_audio = config.clone();
    audio_combo.connect_changed(move |combo| {
        let selected = combo.active_id().map(|s| s.to_string());
        {
            let mut cfg = config_for_audio.lock().unwrap();
            cfg.audio_source = selected.filter(|value| value != "default");
            let _ = cfg.save();
        }
    });

    let config_for_mode = config.clone();
    mode_combo.connect_changed(move |combo| {
        let mode = match combo.active_id().as_deref() {
            Some("clipboard") => OutputMode::ClipboardPaste,
            _ => OutputMode::DirectTyping,
        };
        {
            let mut cfg = config_for_mode.lock().unwrap();
            cfg.output_mode = mode;
            let _ = cfg.save();
        }
    });

    let config_for_model = config.clone();
    let window_for_model = window.clone();
    model_combo.connect_changed(move |combo| {
        let Some(id) = combo.active_id().map(|v| v.to_string()) else {
            return;
        };
        if id == "other" {
            let chooser = FileChooserNative::builder()
                .title("Select ggml model")
                .action(FileChooserAction::Open)
                .transient_for(&window_for_model)
                .accept_label("Select")
                .cancel_label("Cancel")
                .build();
            chooser.connect_response({
                let combo = combo.clone();
                let config_for_model = config_for_model.clone();
                move |dialog, response| {
                    if response == ResponseType::Accept {
                        if let Some(file) = dialog.file().and_then(|f| f.path()) {
                            let path = file.to_string_lossy().to_string();
                            combo.append(Some(&path), &path);
                            combo.set_active_id(Some(&path));
                            let mut cfg = config_for_model.lock().unwrap();
                            cfg.model_path = Some(path);
                            let _ = cfg.save();
                        }
                    }
                    dialog.destroy();
                }
            });
            chooser.show();
            return;
        }
        let mut cfg = config_for_model.lock().unwrap();
        cfg.model_path = Some(id);
        let _ = cfg.save();
    });

    let config_for_duration = config.clone();
    duration_spin.connect_value_changed(move |spin| {
        let value = spin.value().round().clamp(5.0, 180.0) as u32;
        {
            let mut cfg = config_for_duration.lock().unwrap();
            cfg.max_recording_secs = value;
            let _ = cfg.save();
        }
    });

    root.append(&hotkey_heading);
    root.append(&hotkey_button);
    root.append(&capture_hint);
    root.append(&hotkey_value);
    root.append(&audio_heading);
    root.append(&audio_combo);
    root.append(&mode_heading);
    root.append(&mode_combo);
    root.append(&duration_heading);
    root.append(&duration_spin);
    root.append(&model_heading);
    root.append(&model_combo);

    window.set_child(Some(&root));
    window
}
