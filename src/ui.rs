use crate::config::{AppConfig, OutputMode};
use crate::services::{
    default_whisper_model_path, list_audio_sources, list_whisper_models,
    validate_whisper_model_path, Hotkey,
};
use gtk::gdk;
use gtk::pango::EllipsizeMode;
use gtk::prelude::*;
use gtk::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, ButtonsType, CssProvider,
    DrawingArea, DropDown, FileChooserAction, FileChooserNative, Frame, Grid, Image, Label,
    MessageDialog, MessageType, Orientation, ResponseType, SignalListItemFactory, SpinButton,
    StringList, StringObject, STYLE_PROVIDER_PRIORITY_APPLICATION,
};
use std::cell::RefCell;
use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

const OVERLAY_BAR_COUNT: usize = 11;

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
    levels: VecDeque<f32>,
    level: f32,
}

fn show_model_validation_error(parent: &ApplicationWindow, model_path: &str, error: &str) {
    let dialog = MessageDialog::builder()
        .transient_for(parent)
        .modal(true)
        .message_type(MessageType::Error)
        .buttons(ButtonsType::Ok)
        .text("Failed to load the selected Whisper model.")
        .secondary_text(format!("{model_path}\n\n{error}"))
        .build();
    dialog.connect_response(|dialog, _| dialog.close());
    dialog.present();
}

fn ellipsized_dropdown(model: &StringList, ellipsize: EllipsizeMode) -> DropDown {
    let factory = SignalListItemFactory::new();
    factory.connect_setup(move |_, item| {
        let label = Label::new(None);
        label.set_hexpand(true);
        label.set_xalign(0.0);
        label.set_ellipsize(ellipsize);
        let list_item = item
            .downcast_ref::<gtk::ListItem>()
            .expect("drop-down setup item must be a GtkListItem");
        list_item.set_child(Some(&label));
    });
    factory.connect_bind(|_, item| {
        let list_item = item
            .downcast_ref::<gtk::ListItem>()
            .expect("drop-down bind item must be a GtkListItem");
        let label = list_item
            .child()
            .and_then(|child| child.downcast::<Label>().ok())
            .expect("drop-down list item child must be a GtkLabel");
        let text = list_item
            .item()
            .and_then(|obj| obj.downcast::<StringObject>().ok())
            .map(|obj| obj.string())
            .unwrap_or_default();
        label.set_label(&text);
    });

    let dropdown = DropDown::new(Some(model.clone()), None::<&gtk::Expression>);
    dropdown.set_factory(Some(&factory));
    dropdown.set_list_factory(Some(&factory));
    dropdown.set_hexpand(true);
    dropdown
}

fn dropdown_index_for_value(model: &StringList, value: &str) -> Option<u32> {
    (0..model.n_items()).find(|&index| model.string(index).as_deref() == Some(value))
}

fn dropdown_select_value(dropdown: &DropDown, model: &StringList, value: &str) -> bool {
    if let Some(index) = dropdown_index_for_value(model, value) {
        dropdown.set_selected(index);
        true
    } else {
        false
    }
}

pub(crate) fn audio_source_selection_to_config(selection: &str) -> Option<String> {
    (selection != "Default source").then(|| selection.to_string())
}

pub(crate) fn output_mode_selection_to_config(selection: &str) -> Option<OutputMode> {
    match selection {
        "Direct typing" => Some(OutputMode::DirectTyping),
        "Clipboard paste" => Some(OutputMode::ClipboardPaste),
        _ => None,
    }
}

fn with_model_selection_guard(guard: &Arc<Mutex<bool>>, f: impl FnOnce()) {
    {
        let mut active = guard.lock().unwrap();
        *active = true;
    }
    f();
    {
        let mut active = guard.lock().unwrap();
        *active = false;
    }
}

fn build_settings_section(title: &str, icon_name: &str) -> (Frame, GtkBox) {
    let frame = Frame::new(None);
    frame.add_css_class("whisper-settings-card");
    frame.set_hexpand(true);

    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);

    let header = GtkBox::new(Orientation::Horizontal, 8);
    header.set_hexpand(true);
    let icon = Image::from_icon_name(icon_name);
    icon.set_pixel_size(16);
    let title_label = Label::new(Some(title));
    title_label.add_css_class("whisper-settings-section-title");
    title_label.set_halign(Align::Start);
    title_label.set_hexpand(true);
    header.append(&icon);
    header.append(&title_label);

    let body = GtkBox::new(Orientation::Vertical, 10);
    body.set_hexpand(true);

    content.append(&header);
    content.append(&body);
    frame.set_child(Some(&content));

    (frame, body)
}

fn build_dropdown_from_strings(
    values: &[&str],
    ellipsize: EllipsizeMode,
) -> (StringList, DropDown) {
    let list = StringList::new(values);
    let dropdown = ellipsized_dropdown(&list, ellipsize);
    (list, dropdown)
}

fn default_model_selection(model_paths: &[std::path::PathBuf], cfg: &AppConfig) -> String {
    if let Some(path) = cfg.model_path.as_deref() {
        let path = path.to_string();
        if model_paths
            .iter()
            .any(|model_path| model_path.to_string_lossy() == path)
        {
            return path;
        }
    }

    let default = default_whisper_model_path().to_string_lossy().to_string();
    if model_paths
        .iter()
        .any(|model_path| model_path.to_string_lossy() == default)
    {
        return default;
    }

    model_paths
        .first()
        .map(|model_path| model_path.to_string_lossy().to_string())
        .unwrap_or_else(|| "Other...".to_string())
}

fn install_settings_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };

    let provider = CssProvider::new();
    provider.load_from_data(
        r#"
        .whisper-settings-card {
            background-color: @theme_base_color;
            border: 1px solid alpha(@theme_fg_color, 0.10);
            border-radius: 10px;
        }

        .whisper-settings-section-title {
            font-weight: 700;
        }

        "#,
    );

    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn validate_model_selection(
    config: Arc<Mutex<AppConfig>>,
    parent: gtk::glib::SendWeakRef<ApplicationWindow>,
    dropdown: gtk::glib::SendWeakRef<DropDown>,
    model_list: gtk::glib::SendWeakRef<StringList>,
    selection_guard: Arc<Mutex<bool>>,
    model_path: String,
    custom_entry: bool,
    previous_path: String,
) {
    std::thread::spawn(move || {
        let result =
            validate_whisper_model_path(Path::new(&model_path)).map_err(|err| err.to_string());
        gtk::glib::MainContext::default().invoke(move || {
            let Some(parent) = parent.upgrade() else {
                return;
            };
            let Some(dropdown) = dropdown.upgrade() else {
                return;
            };
            let Some(model_list) = model_list.upgrade() else {
                return;
            };
            match result {
                Ok(()) => {
                    {
                        let mut cfg = config.lock().unwrap();
                        cfg.model_path = Some(model_path.clone());
                        let _ = cfg.save();
                    }
                    if custom_entry && dropdown_index_for_value(&model_list, &model_path).is_none()
                    {
                        model_list.append(&model_path);
                    }
                    with_model_selection_guard(&selection_guard, || {
                        let _ = dropdown_select_value(&dropdown, &model_list, &model_path);
                    });
                }
                Err(error) => {
                    show_model_validation_error(&parent, &model_path, &error);
                    with_model_selection_guard(&selection_guard, || {
                        let _ = dropdown_select_value(&dropdown, &model_list, &previous_path);
                    });
                }
            }
        });
    });
}

impl Default for WaveformState {
    fn default() -> Self {
        Self {
            levels: std::iter::repeat(0.0).take(OVERLAY_BAR_COUNT).collect(),
            level: 0.0,
        }
    }
}

impl WaveformState {
    pub fn set_level(&mut self, level: f32) {
        self.level = level.clamp(0.0, 1.0);
    }

    pub fn advance(&mut self) {
        if self.levels.len() == OVERLAY_BAR_COUNT {
            self.levels.pop_front();
        }
        self.levels.push_back(self.level);
    }

    pub fn bar_heights(&self, bar_count: usize) -> Vec<f64> {
        let values = if self.levels.len() >= bar_count {
            self.levels
                .iter()
                .copied()
                .skip(self.levels.len() - bar_count)
                .collect::<Vec<_>>()
        } else {
            let mut padded = vec![0.0; bar_count - self.levels.len()];
            padded.extend(self.levels.iter().copied());
            padded
        };

        values
            .into_iter()
            .map(|level| (0.16 + (level as f64) * 0.74).clamp(0.08, 1.0))
            .collect()
    }
}

impl OverlayMeter {
    pub fn set_level(&self, level: f32) {
        if let Ok(mut state) = self.state.try_borrow_mut() {
            state.set_level(level);
        }
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
    install_settings_css();
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Whisper Settings")
        .default_width(430)
        .default_height(50)
        .resizable(false)
        .build();
    window.set_hide_on_close(true);

    let root = GtkBox::new(Orientation::Vertical, 0);
    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_margin_top(16);
    content.set_margin_bottom(16);
    content.set_margin_start(16);
    content.set_margin_end(16);
    content.set_hexpand(true);

    let programmatic_update = Rc::new(RefCell::new(false));

    let hotkey_button = Button::new();
    hotkey_button.set_hexpand(true);
    hotkey_button.set_halign(Align::Fill);

    let (mode_list, mode_dropdown) =
        build_dropdown_from_strings(&["Direct typing", "Clipboard paste"], EllipsizeMode::End);
    mode_dropdown.set_hexpand(true);
    mode_dropdown.set_halign(Align::Fill);

    let audio_sources = StringList::new(&[]);
    audio_sources.append("Default source");
    for source in list_audio_sources() {
        audio_sources.append(&source);
    }
    let audio_dropdown = ellipsized_dropdown(&audio_sources, EllipsizeMode::End);
    audio_dropdown.set_halign(Align::Fill);

    let duration_spin = SpinButton::with_range(5.0, 180.0, 1.0);
    duration_spin.set_hexpand(true);
    duration_spin.set_halign(Align::End);
    duration_spin.set_width_chars(5);

    let threads_spin = SpinButton::with_range(1.0, 128.0, 1.0);
    threads_spin.set_hexpand(true);
    threads_spin.set_halign(Align::End);
    threads_spin.set_width_chars(5);

    let model_paths = list_whisper_models();
    let model_entries = StringList::new(&[]);
    for model_path in &model_paths {
        let model_path = model_path.to_string_lossy();
        model_entries.append(model_path.as_ref());
    }
    model_entries.append("Other...");
    let model_dropdown = ellipsized_dropdown(&model_entries, EllipsizeMode::Start);
    model_dropdown.set_halign(Align::Fill);
    let model_browse_button = Button::with_label("Browse...");
    let model_selection_guard = Arc::new(Mutex::new(false));

    let (general_card, general_body) =
        build_settings_section("General", "preferences-system-symbolic");
    let (audio_card, audio_body) =
        build_settings_section("Audio", "audio-input-microphone-symbolic");
    let (performance_card, performance_body) =
        build_settings_section("Performance", "utilities-system-monitor-symbolic");
    let (model_card, model_body) = build_settings_section("Model", "package-x-generic-symbolic");

    let general_grid = Grid::new();
    general_grid.set_column_spacing(12);
    general_grid.set_row_spacing(12);
    general_grid.set_hexpand(true);

    let hotkey_label = Label::new(Some("Hotkey"));
    hotkey_label.set_halign(Align::Start);
    hotkey_label.set_valign(Align::Center);
    let output_mode_label = Label::new(Some("Output Mode"));
    output_mode_label.set_halign(Align::Start);
    output_mode_label.set_valign(Align::Center);
    general_grid.attach(&hotkey_label, 0, 0, 1, 1);
    general_grid.attach(&hotkey_button, 1, 0, 1, 1);
    general_grid.attach(&output_mode_label, 0, 1, 1, 1);
    general_grid.attach(&mode_dropdown, 1, 1, 1, 1);
    general_body.append(&general_grid);

    audio_body.append(&audio_dropdown);

    let performance_grid = Grid::new();
    performance_grid.set_column_spacing(12);
    performance_grid.set_row_spacing(12);
    performance_grid.set_hexpand(true);

    let duration_label = Label::new(Some("Max Recording Duration (s)"));
    duration_label.set_halign(Align::Start);
    duration_label.set_valign(Align::Center);
    let threads_label = Label::new(Some("Whisper CPU Threads"));
    threads_label.set_halign(Align::Start);
    threads_label.set_valign(Align::Center);
    performance_grid.attach(&duration_label, 0, 0, 1, 1);
    performance_grid.attach(&duration_spin, 1, 0, 1, 1);
    performance_grid.attach(&threads_label, 0, 1, 1, 1);
    performance_grid.attach(&threads_spin, 1, 1, 1, 1);
    performance_body.append(&performance_grid);

    let model_grid = Grid::new();
    model_grid.set_column_spacing(12);
    model_grid.set_row_spacing(12);
    model_grid.set_hexpand(true);

    model_grid.attach(&model_dropdown, 0, 0, 1, 1);
    model_grid.attach(&model_browse_button, 1, 0, 1, 1);
    model_body.append(&model_grid);

    let cfg = config.lock().unwrap().clone();
    let current_hotkey = Hotkey::parse(&cfg.hotkey)
        .map(|h| h.display())
        .unwrap_or_else(|| cfg.hotkey.clone());
    hotkey_button.set_label(&current_hotkey);
    let current_audio = cfg.audio_source.as_deref().unwrap_or("Default source");
    let _ = dropdown_select_value(&audio_dropdown, &audio_sources, current_audio);
    let mode_value = match cfg.output_mode {
        OutputMode::DirectTyping => "Direct typing",
        OutputMode::ClipboardPaste => "Clipboard paste",
    };
    let _ = dropdown_select_value(&mode_dropdown, &mode_list, mode_value);
    duration_spin.set_value(cfg.max_recording_secs as f64);
    threads_spin.set_value(cfg.whisper_threads.max(1) as f64);
    let current_model = default_model_selection(&model_paths, &cfg);
    let _ = dropdown_select_value(&model_dropdown, &model_entries, &current_model);

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
    let window_for_keys = window.clone();
    controller.connect_key_pressed(move |_, key, _keycode, state| {
        if key == gdk::Key::Escape {
            if *capture_mode_for_keys.borrow() {
                *capture_mode_for_keys.borrow_mut() = false;
                if let Some(cfg) = Hotkey::parse(&config_for_keys.lock().unwrap().hotkey) {
                    hotkey_button_for_keys.set_label(&cfg.display());
                }
            } else {
                window_for_keys.close();
            }
            return gtk::glib::Propagation::Stop;
        }

        if !*capture_mode_for_keys.borrow() {
            return gtk::glib::Propagation::Proceed;
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
            return gtk::glib::Propagation::Stop;
        }

        gtk::glib::Propagation::Proceed
    });
    window.add_controller(controller);

    let config_for_audio = config.clone();
    let audio_sources_for_combo = audio_sources.clone();
    audio_dropdown.connect_selected_notify(move |combo| {
        let selected = combo.selected();
        if selected == gtk::INVALID_LIST_POSITION {
            return;
        }
        let Some(value) = audio_sources_for_combo
            .string(selected)
            .map(|source| source.to_string())
        else {
            return;
        };
        let new_source = audio_source_selection_to_config(&value);
        let mut cfg = config_for_audio.lock().unwrap();
        if cfg.audio_source == new_source {
            return;
        }
        cfg.audio_source = new_source;
        let _ = cfg.save();
    });

    let config_for_mode = config.clone();
    let mode_list_for_combo = mode_list.clone();
    mode_dropdown.connect_selected_notify(move |combo| {
        let selected = combo.selected();
        if selected == gtk::INVALID_LIST_POSITION {
            return;
        }
        let Some(value) = mode_list_for_combo
            .string(selected)
            .map(|mode| mode.to_string())
        else {
            return;
        };
        let Some(new_mode) = output_mode_selection_to_config(&value) else {
            return;
        };
        let mut cfg = config_for_mode.lock().unwrap();
        if cfg.output_mode == new_mode {
            return;
        }
        cfg.output_mode = new_mode;
        let _ = cfg.save();
    });

    let config_for_model = config.clone();
    let window_for_model = window.clone();
    let model_entries_for_picker = model_entries.clone();
    let model_dropdown_for_picker = model_dropdown.clone();
    let model_selection_guard_for_picker = model_selection_guard.clone();
    let open_model_picker = {
        let config_for_model = config_for_model.clone();
        let window_for_model = window_for_model.clone();
        let model_dropdown = model_dropdown_for_picker.clone();
        let model_entries = model_entries_for_picker.clone();
        let model_selection_guard = model_selection_guard_for_picker.clone();
        Rc::new(move || {
            let previous_selection = config_for_model
                .lock()
                .unwrap()
                .model_path
                .clone()
                .unwrap_or_else(|| default_whisper_model_path().to_string_lossy().to_string());
            with_model_selection_guard(&model_selection_guard, || {
                let _ = dropdown_select_value(&model_dropdown, &model_entries, &previous_selection);
            });
            let chooser = FileChooserNative::builder()
                .title("Select ggml model")
                .action(FileChooserAction::Open)
                .transient_for(&window_for_model)
                .accept_label("Select")
                .cancel_label("Cancel")
                .build();
            chooser.connect_response({
                let parent = window_for_model.clone();
                let dropdown = model_dropdown.clone();
                let model_entries = model_entries.clone();
                let model_selection_guard = model_selection_guard.clone();
                let config_for_model = config_for_model.clone();
                move |dialog, response| {
                    if response == ResponseType::Accept {
                        if let Some(file) = dialog.file().and_then(|f| f.path()) {
                            let parent: gtk::glib::SendWeakRef<ApplicationWindow> =
                                parent.downgrade().into();
                            let combo: gtk::glib::SendWeakRef<DropDown> =
                                dropdown.downgrade().into();
                            let model_entries: gtk::glib::SendWeakRef<StringList> =
                                model_entries.downgrade().into();
                            validate_model_selection(
                                config_for_model.clone(),
                                parent.clone(),
                                combo.clone(),
                                model_entries.clone(),
                                model_selection_guard.clone(),
                                file.to_string_lossy().to_string(),
                                true,
                                previous_selection.clone(),
                            );
                        } else {
                            with_model_selection_guard(&model_selection_guard, || {
                                let _ = dropdown_select_value(
                                    &dropdown,
                                    &model_entries,
                                    &previous_selection,
                                );
                            });
                        }
                    } else {
                        with_model_selection_guard(&model_selection_guard, || {
                            let _ = dropdown_select_value(
                                &dropdown,
                                &model_entries,
                                &previous_selection,
                            );
                        });
                    }
                    dialog.destroy();
                }
            });
            chooser.show();
        })
    };
    let open_model_picker_for_combo = open_model_picker.clone();
    let model_entries_for_combo = model_entries.clone();
    let model_selection_guard_for_combo = model_selection_guard.clone();
    let programmatic_update_for_combo = programmatic_update.clone();
    model_dropdown.connect_selected_notify(move |combo| {
        if *programmatic_update_for_combo.borrow() {
            return;
        }
        if *model_selection_guard_for_combo.lock().unwrap() {
            return;
        }
        let selected = combo.selected();
        if selected == gtk::INVALID_LIST_POSITION {
            return;
        }
        let Some(id) = model_entries_for_combo
            .string(selected)
            .map(|s| s.to_string())
        else {
            return;
        };
        if id == "Other..." {
            open_model_picker_for_combo();
            return;
        }
        let current_model = config_for_model
            .lock()
            .unwrap()
            .model_path
            .clone()
            .unwrap_or_else(|| default_whisper_model_path().to_string_lossy().to_string());
        if current_model == id {
            return;
        }
        let current_model_for_restore = current_model.clone();
        let parent = window_for_model.downgrade().into();
        let combo = combo.downgrade().into();
        let model_entries = model_entries_for_combo.downgrade().into();
        validate_model_selection(
            config_for_model.clone(),
            parent,
            combo,
            model_entries,
            model_selection_guard.clone(),
            id,
            false,
            current_model_for_restore,
        );
    });

    let open_model_picker_for_button = open_model_picker.clone();
    model_browse_button.connect_clicked(move |_| {
        open_model_picker_for_button();
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

    let config_for_threads = config.clone();
    threads_spin.connect_value_changed(move |spin| {
        let value = spin.value().round().max(1.0) as u32;
        {
            let mut cfg = config_for_threads.lock().unwrap();
            cfg.whisper_threads = value;
            let _ = cfg.save();
        }
    });

    content.append(&general_card);
    content.append(&audio_card);
    content.append(&performance_card);
    content.append(&model_card);

    root.append(&content);

    window.set_child(Some(&root));
    window
}
