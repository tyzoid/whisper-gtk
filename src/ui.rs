use crate::config::{AppConfig, OutputMode};
use crate::downloads::{
    download_whisper_models, list_downloadable_whisper_models, model_selection_after_refresh,
    whisper_download_cache_dir, DownloadableWhisperModel, WhisperModelDownloadEvent,
    WhisperModelDownloadStatus,
};
use crate::services::{
    default_whisper_model_path, list_audio_sources, list_whisper_models,
    validate_whisper_model_path_in_subprocess, Hotkey,
};
use glib::object::ObjectType as _;
use gtk::gdk;
use gtk::gdk::prelude::*;
use gtk::pango::EllipsizeMode;
use gtk::prelude::*;
use gtk::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, ButtonsType, CenterBox,
    CheckButton, CssProvider, DrawingArea, DropDown, Entry, FileChooserAction, FileChooserNative,
    Frame, GestureClick, Grid, Image, Label, MessageDialog, MessageType, Orientation, ProgressBar,
    ResponseType, ScrolledWindow, SignalListItemFactory, SpinButton, Stack, StringList,
    StringObject, STYLE_PROVIDER_PRIORITY_APPLICATION,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::f64::consts::TAU;
use std::ffi::c_void;
use std::os::raw::{c_int, c_long, c_uchar, c_ulong};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};

const OVERLAY_BAR_COUNT: usize = 11;

#[repr(C)]
struct XDisplay(c_void);

#[link(name = "gtk-4")]
extern "C" {
    fn gdk_x11_display_get_xdisplay(display: *mut c_void) -> *mut XDisplay;
    fn gdk_x11_surface_get_xid(surface: *mut c_void) -> c_ulong;
    fn gdk_x11_surface_set_skip_taskbar_hint(surface: *mut c_void, skips_taskbar: c_int);
    fn gdk_x11_surface_set_skip_pager_hint(surface: *mut c_void, skips_pager: c_int);
}

#[link(name = "X11")]
extern "C" {
    fn XRaiseWindow(display: *mut XDisplay, window: c_ulong) -> c_int;
    fn XDefaultRootWindow(display: *mut XDisplay) -> c_ulong;
    fn XInternAtom(
        display: *mut XDisplay,
        atom_name: *const c_uchar,
        only_if_exists: c_int,
    ) -> c_ulong;
    fn XChangeProperty(
        display: *mut XDisplay,
        w: c_ulong,
        property: c_ulong,
        type_: c_ulong,
        format: c_int,
        mode: c_int,
        data: *const c_uchar,
        nelements: c_int,
    );
    fn XFlush(display: *mut XDisplay);
    fn XSendEvent(
        display: *mut XDisplay,
        w: c_ulong,
        propagate: c_int,
        event_mask: c_long,
        event_send: *mut XEvent,
    ) -> c_int;
}

#[repr(C)]
#[derive(Clone, Copy)]
union XClientMessageData {
    l: [c_long; 5],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct XClientMessageEvent {
    type_: c_int,
    serial: c_ulong,
    send_event: c_int,
    display: *mut XDisplay,
    window: c_ulong,
    message_type: c_ulong,
    format: c_int,
    data: XClientMessageData,
}

#[repr(C)]
union XEvent {
    type_: c_int,
    xclient: XClientMessageEvent,
    pad: [c_long; 24],
}

const XA_ATOM: c_ulong = 4;
const PROP_MODE_REPLACE: c_int = 0;
const CLIENT_MESSAGE: c_int = 33;
const SUBSTRUCTURE_NOTIFY_MASK: c_long = 1 << 19;
const SUBSTRUCTURE_REDIRECT_MASK: c_long = 1 << 20;
const NET_WM_STATE_ADD: c_long = 1;
const NET_WM_STATE_SOURCE_APPLICATION: c_long = 1;
type ModelWarningIcons = Arc<Mutex<HashMap<String, Vec<gtk::glib::SendWeakRef<Image>>>>>;
const DOWNLOAD_CHEVRON_COLUMN_WIDTH: i32 = 16;
const DOWNLOAD_MODEL_COLUMN_WIDTH: i32 = 268;
const DOWNLOAD_QUANT_COLUMN_WIDTH: i32 = 150;
const DOWNLOAD_SIZE_COLUMN_WIDTH: i32 = 92;
const DOWNLOAD_STATUS_COLUMN_WIDTH: i32 = 116;
const OVERLAY_MIC_ICON_HEIGHT: i32 = 20;
const OVERLAY_MIC_ICON_ASPECT_RATIO: f64 = 462.86 / 688.86175;
const OVERLAY_MIC_ICON_X: f64 = 16.0;
const OVERLAY_MIC_ICON_Y: f64 = 10.0;
const OVERLAY_WAVEFORM_X_PADDING: f64 = 12.0;
const OVERLAY_WAVEFORM_RIGHT_PADDING: f64 = 10.0;

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

pub(crate) fn prepare_overlay_window(window: &ApplicationWindow) {
    window.set_can_target(false);
    window.set_focus_on_click(false);

    let Some(native) = window.native() else {
        return;
    };
    let Some(surface) = native.surface() else {
        return;
    };

    let display = surface.display();
    if display.supports_input_shapes() {
        // Make the overlay transparent to pointer hits so clicks reach the app below it.
        surface.set_input_region(&gtk::cairo::Region::create());
    }

    if display.backend().is_x11() {
        unsafe {
            let surface: &gdk::Surface = surface.as_ref();
            let display: &gdk::Display = display.as_ref();
            let surface_ptr = surface.as_ptr() as *mut c_void;
            let display_ptr = display.as_ptr() as *mut c_void;
            let xdisplay = gdk_x11_display_get_xdisplay(display_ptr);
            if xdisplay.is_null() {
                return;
            }

            let net_wm_state =
                XInternAtom(xdisplay, c"_NET_WM_STATE".as_ptr() as *const c_uchar, 0);
            let net_wm_state_above = XInternAtom(
                xdisplay,
                c"_NET_WM_STATE_ABOVE".as_ptr() as *const c_uchar,
                0,
            );
            let net_wm_state_sticky = XInternAtom(
                xdisplay,
                c"_NET_WM_STATE_STICKY".as_ptr() as *const c_uchar,
                0,
            );
            let net_wm_window_type = XInternAtom(
                xdisplay,
                c"_NET_WM_WINDOW_TYPE".as_ptr() as *const c_uchar,
                0,
            );
            let net_wm_window_type_utility = XInternAtom(
                xdisplay,
                c"_NET_WM_WINDOW_TYPE_UTILITY".as_ptr() as *const c_uchar,
                0,
            );

            let xid = gdk_x11_surface_get_xid(surface_ptr);
            if xid != 0 {
                let above_states = [net_wm_state_above, net_wm_state_sticky];
                XChangeProperty(
                    xdisplay,
                    xid,
                    net_wm_state,
                    XA_ATOM,
                    32,
                    PROP_MODE_REPLACE,
                    above_states.as_ptr() as *const c_uchar,
                    above_states.len() as c_int,
                );

                let utility_type = [net_wm_window_type_utility];
                XChangeProperty(
                    xdisplay,
                    xid,
                    net_wm_window_type,
                    XA_ATOM,
                    32,
                    PROP_MODE_REPLACE,
                    utility_type.as_ptr() as *const c_uchar,
                    utility_type.len() as c_int,
                );

                let mut event = XEvent { pad: [0; 24] };
                event.xclient = XClientMessageEvent {
                    type_: CLIENT_MESSAGE,
                    serial: 0,
                    send_event: 1,
                    display: xdisplay,
                    window: xid,
                    message_type: net_wm_state,
                    format: 32,
                    data: XClientMessageData {
                        l: [
                            NET_WM_STATE_ADD,
                            net_wm_state_above as c_long,
                            0,
                            NET_WM_STATE_SOURCE_APPLICATION,
                            0,
                        ],
                    },
                };
                let root = XDefaultRootWindow(xdisplay);
                let _ = XSendEvent(
                    xdisplay,
                    root,
                    0,
                    SUBSTRUCTURE_NOTIFY_MASK | SUBSTRUCTURE_REDIRECT_MASK,
                    &mut event,
                );
            }

            gdk_x11_surface_set_skip_taskbar_hint(surface_ptr, 1);
            gdk_x11_surface_set_skip_pager_hint(surface_ptr, 1);
            if xid != 0 {
                let _ = XRaiseWindow(xdisplay, xid);
            }
            XFlush(xdisplay);
        }
    }
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

fn refresh_model_dropdown_items(
    config: &Arc<Mutex<AppConfig>>,
    model_paths: Vec<PathBuf>,
    model_entries: &StringList,
    model_dropdown: &DropDown,
    invalid_models: &Arc<Mutex<HashMap<String, bool>>>,
    warning_icons: &ModelWarningIcons,
    selection_guard: &Arc<Mutex<bool>>,
) {
    let current_model = config.lock().unwrap().model_path.clone();
    let mut dropdown_paths = model_paths.clone();

    if let Some(current_model) = current_model.as_deref() {
        let has_model = dropdown_paths
            .iter()
            .any(|path| path.to_string_lossy() == current_model);
        if !has_model {
            dropdown_paths.push(PathBuf::from(current_model));
        }
    }

    let item_count = model_entries.n_items();
    if item_count > 0 {
        model_entries.splice(0, item_count, &[]);
    }
    for model_path in &dropdown_paths {
        model_entries.append(&model_path.to_string_lossy());
    }
    model_entries.append("Other...");

    {
        invalid_models.lock().unwrap().clear();
        warning_icons.lock().unwrap().clear();
    }

    let selection = model_selection_after_refresh(&dropdown_paths, current_model.as_deref());
    with_model_selection_guard(selection_guard, || {
        let _ = dropdown_select_value(model_dropdown, model_entries, &selection);
    });

    validate_whisper_models_on_load(
        dropdown_paths,
        invalid_models.clone(),
        warning_icons.clone(),
    );
}

#[derive(Clone)]
struct ModelRowWidgets {
    model: DownloadableWhisperModel,
    row: GtkBox,
    check_button: CheckButton,
    status_stack: Stack,
    queued_progress: ProgressBar,
    status_progress: ProgressBar,
    status_label: Label,
}

impl ModelRowWidgets {
    fn is_selected(&self) -> bool {
        self.check_button.is_active()
    }

    fn set_visible(&self, visible: bool) {
        self.row.set_visible(visible);
    }

    fn set_status(&self, status: WhisperModelDownloadStatus) {
        match status {
            WhisperModelDownloadStatus::Idle => {
                self.status_label.set_label("—");
                self.status_label.set_tooltip_text(None);
                self.status_stack.set_visible_child_name("label");
            }
            WhisperModelDownloadStatus::Queued => {
                self.status_label.set_tooltip_text(None);
                self.queued_progress.pulse();
                self.status_stack.set_visible_child_name("queued");
            }
            WhisperModelDownloadStatus::Downloading {
                downloaded_bytes,
                total_bytes,
            } => {
                self.status_label.set_tooltip_text(None);
                if let Some(total_bytes) = total_bytes.filter(|total| *total > 0) {
                    self.status_progress.set_fraction(
                        (downloaded_bytes as f64 / total_bytes as f64).clamp(0.0, 1.0),
                    );
                    self.status_progress.set_show_text(false);
                } else {
                    self.status_progress.pulse();
                }
                self.status_stack.set_visible_child_name("progress");
            }
            WhisperModelDownloadStatus::Downloaded => {
                self.status_label.set_label("Downloaded");
                self.status_label.set_tooltip_text(None);
                self.status_stack.set_visible_child_name("label");
            }
            WhisperModelDownloadStatus::Skipped => {
                self.status_label.set_label("Already downloaded");
                self.status_label.set_tooltip_text(None);
                self.status_stack.set_visible_child_name("label");
            }
            WhisperModelDownloadStatus::Failed(error) => {
                self.status_label.set_label("Failed");
                self.status_label.set_tooltip_text(Some(&error));
                self.status_stack.set_visible_child_name("label");
            }
        }
    }
}

#[derive(Clone)]
struct GroupWidgets {
    header: GtkBox,
    expand_icon: Image,
    select_all: CheckButton,
    children: GtkBox,
    expanded: Rc<RefCell<bool>>,
    row_indices: Vec<usize>,
}

#[derive(Clone)]
struct DownloadDialogState {
    rows: Vec<ModelRowWidgets>,
    groups: Vec<GroupWidgets>,
    global_select_all: CheckButton,
    download_button: Button,
    search_entry: Entry,
    selection_guard: Rc<RefCell<bool>>,
    queued_model_ids: Rc<RefCell<HashSet<&'static str>>>,
}

impl DownloadDialogState {
    fn selected_count(&self) -> usize {
        self.rows.iter().filter(|row| row.is_selected()).count()
    }

    fn eligible_selected_models(&self) -> Vec<DownloadableWhisperModel> {
        self.rows
            .iter()
            .filter(|row| row.is_selected())
            .map(|row| row.model)
            .collect()
    }

    fn update_action_labels(&self) {
        let count = self.selected_count();
        self.download_button
            .set_label(&format!("Download Selected ({count})"));
        self.download_button.set_sensitive(count > 0);
    }

    fn with_selection_guard(&self, f: impl FnOnce()) {
        {
            let mut guard = self.selection_guard.borrow_mut();
            if *guard {
                return;
            }
            *guard = true;
        }
        f();
        *self.selection_guard.borrow_mut() = false;
    }

    fn update_group_checks(&self) {
        let any_selected = self.rows.iter().any(|row| row.is_selected());
        self.with_selection_guard(|| {
            self.global_select_all
                .set_active(any_selected && self.rows.iter().all(|row| row.is_selected()));
            self.global_select_all
                .set_inconsistent(any_selected && !self.rows.iter().all(|row| row.is_selected()));

            for group in &self.groups {
                let rows = group
                    .row_indices
                    .iter()
                    .filter_map(|index| self.rows.get(*index))
                    .collect::<Vec<_>>();
                let selected = rows.iter().filter(|row| row.is_selected()).count();
                let all_selected = !rows.is_empty() && selected == rows.len();
                let some_selected = selected > 0;
                group.select_all.set_active(all_selected);
                group
                    .select_all
                    .set_inconsistent(some_selected && !all_selected);
            }
        });
    }

    fn apply_filter(&self) {
        let query = self.search_entry.text().to_string();
        let query = query.trim().to_lowercase();

        for group in &self.groups {
            let expanded = *group.expanded.borrow();
            let mut any_match = false;

            for index in &group.row_indices {
                let row = &self.rows[*index];
                let matches = query.is_empty() || row.model.matches_query(&query);
                any_match |= matches;
                row.set_visible(matches);
            }

            let group_visible = query.is_empty() || any_match;
            group.header.set_visible(group_visible);
            group
                .children
                .set_visible(group_visible && (expanded || !query.is_empty()));
            group.expand_icon.set_icon_name(Some(if expanded {
                "pan-down-symbolic"
            } else {
                "pan-end-symbolic"
            }));
        }
    }

    fn select_all_models(&self, selected: bool) {
        self.with_selection_guard(|| {
            for row in &self.rows {
                row.check_button.set_active(selected);
            }
        });
        self.update_group_checks();
        self.update_action_labels();
    }

    fn update_from_row_toggle(&self) {
        self.update_group_checks();
        self.update_action_labels();
    }
}

fn build_downloadable_model_row(
    model: DownloadableWhisperModel,
    download_destination: &Path,
) -> ModelRowWidgets {
    let row = GtkBox::new(Orientation::Horizontal, 12);
    row.set_hexpand(true);
    row.add_css_class("whisper-download-row");
    row.set_margin_start(0);
    row.set_margin_end(0);
    row.set_margin_top(0);
    row.set_margin_bottom(0);

    let check_button = CheckButton::new();
    check_button.set_valign(Align::Center);

    let model_label = Label::new(Some(model.model_id));
    model_label.set_hexpand(true);
    model_label.set_xalign(0.0);
    model_label.set_valign(Align::Center);
    model_label.set_width_chars(18);

    let model_cell = GtkBox::new(Orientation::Horizontal, 8);
    model_cell.set_hexpand(false);
    model_cell.set_size_request(DOWNLOAD_MODEL_COLUMN_WIDTH, -1);
    model_cell.append(&check_button);
    model_cell.append(&model_label);

    let quant_label = Label::new(Some(model.quantization));
    quant_label.set_xalign(0.0);
    quant_label.set_valign(Align::Center);
    quant_label.set_size_request(DOWNLOAD_QUANT_COLUMN_WIDTH, -1);

    let size_label = Label::new(Some(model.size_label));
    size_label.set_xalign(1.0);
    size_label.set_valign(Align::Center);
    size_label.set_size_request(DOWNLOAD_SIZE_COLUMN_WIDTH, -1);

    let status_stack = Stack::new();
    status_stack.set_hexpand(false);
    status_stack.set_valign(Align::Center);
    status_stack.set_size_request(DOWNLOAD_STATUS_COLUMN_WIDTH, -1);

    let queued_progress = ProgressBar::new();
    queued_progress.add_css_class("whisper-download-queued-progress");
    queued_progress.set_hexpand(false);
    queued_progress.set_valign(Align::Center);
    queued_progress.set_show_text(false);
    queued_progress.set_pulse_step(0.18);
    queued_progress.set_size_request(DOWNLOAD_STATUS_COLUMN_WIDTH, -1);
    let status_progress = ProgressBar::new();
    status_progress.set_hexpand(false);
    status_progress.set_valign(Align::Center);
    status_progress.set_show_text(false);
    status_progress.set_size_request(DOWNLOAD_STATUS_COLUMN_WIDTH, -1);
    let status_label = Label::new(Some("—"));
    status_label.set_valign(Align::Center);
    status_label.set_xalign(0.0);

    status_stack.add_named(&status_label, Some("label"));
    status_stack.add_named(&queued_progress, Some("queued"));
    status_stack.add_named(&status_progress, Some("progress"));
    status_stack.set_visible_child_name("label");

    row.append(&download_chevron_spacer());
    row.append(&model_cell);
    row.append(&quant_label);
    row.append(&size_label);
    row.append(&status_stack);

    let widgets = ModelRowWidgets {
        model,
        row,
        check_button,
        status_stack,
        queued_progress,
        status_progress,
        status_label,
    };
    if model.destination_path(download_destination).is_file() {
        widgets.set_status(WhisperModelDownloadStatus::Downloaded);
    } else {
        widgets.set_status(WhisperModelDownloadStatus::Idle);
    }
    widgets
}

fn download_chevron_spacer() -> Label {
    let spacer = Label::new(None);
    spacer.set_size_request(DOWNLOAD_CHEVRON_COLUMN_WIDTH, -1);
    spacer
}

fn build_download_group_header(
    family: &'static str,
    expanded: Rc<RefCell<bool>>,
    children: &GtkBox,
    row_indices: Vec<usize>,
) -> GroupWidgets {
    let header = GtkBox::new(Orientation::Horizontal, 12);
    header.set_hexpand(true);
    header.add_css_class("whisper-download-group");
    header.set_margin_start(0);
    header.set_margin_end(0);
    header.set_margin_top(0);
    header.set_margin_bottom(0);

    let clickable_area = GtkBox::new(Orientation::Horizontal, 12);
    clickable_area.set_hexpand(false);
    clickable_area.set_tooltip_text(Some("Expand or collapse this model group"));

    let reveal_cell = CenterBox::new();
    reveal_cell.set_size_request(DOWNLOAD_CHEVRON_COLUMN_WIDTH, -1);
    let expand_icon = Image::from_icon_name("pan-down-symbolic");
    expand_icon.set_halign(Align::Center);
    expand_icon.set_valign(Align::Center);
    expand_icon.set_margin_end(-6);
    expand_icon.set_margin_start(6);
    reveal_cell.set_center_widget(Some(&expand_icon));

    let title = Label::new(Some(family));
    title.set_hexpand(false);
    title.set_xalign(0.0);
    title.set_size_request(DOWNLOAD_MODEL_COLUMN_WIDTH, -1);

    let quant_spacer = Label::new(None);
    quant_spacer.set_size_request(DOWNLOAD_QUANT_COLUMN_WIDTH, -1);

    let group_size_label = group_download_size_label(family);
    let size_label = Label::new(if *expanded.borrow() {
        None
    } else {
        Some(group_size_label.as_str())
    });
    size_label.set_xalign(1.0);
    size_label.set_size_request(DOWNLOAD_SIZE_COLUMN_WIDTH, -1);

    let select_all = CheckButton::with_label("Select all");
    select_all.set_halign(Align::End);
    select_all.set_valign(Align::Center);
    select_all.set_size_request(DOWNLOAD_STATUS_COLUMN_WIDTH, -1);

    clickable_area.append(&reveal_cell);
    clickable_area.append(&title);
    clickable_area.append(&quant_spacer);
    clickable_area.append(&size_label);
    header.append(&clickable_area);
    header.append(&select_all);

    let children_for_toggle = children.clone();
    let expand_icon_for_toggle = expand_icon.clone();
    let size_label_for_toggle = size_label.clone();
    let expanded_for_toggle = expanded.clone();
    let click_controller = GestureClick::new();
    click_controller.connect_released(move |_, _, _, _| {
        let mut expanded = expanded_for_toggle.borrow_mut();
        *expanded = !*expanded;
        children_for_toggle.set_visible(*expanded);
        size_label_for_toggle.set_label(if *expanded { "" } else { &group_size_label });
        expand_icon_for_toggle.set_icon_name(Some(if *expanded {
            "pan-down-symbolic"
        } else {
            "pan-end-symbolic"
        }));
    });
    clickable_area.add_controller(click_controller);

    GroupWidgets {
        header,
        expand_icon,
        select_all,
        children: children.clone(),
        expanded,
        row_indices,
    }
}

fn group_download_size_label(family: &str) -> String {
    let total_bytes = list_downloadable_whisper_models()
        .iter()
        .filter(|model| model.family == family)
        .map(|model| model.size_bytes)
        .sum();
    format_download_size(total_bytes)
}

fn format_download_size(bytes: u64) -> String {
    let (value, unit) = if bytes >= 1_000_000_000 {
        (bytes as f64 / 1_000_000_000.0, "GB")
    } else {
        (bytes as f64 / 1_000_000.0, "MB")
    };

    let label = format!("{value:.2}");
    let label = label.trim_end_matches('0').trim_end_matches('.');
    format!("{label} {unit}")
}

fn open_model_download_window(
    parent: &ApplicationWindow,
    config: Arc<Mutex<AppConfig>>,
    model_dropdown: DropDown,
    model_entries: StringList,
    invalid_models: Arc<Mutex<HashMap<String, bool>>>,
    warning_icons: ModelWarningIcons,
    selection_guard: Arc<Mutex<bool>>,
) {
    let Some(app) = parent.application() else {
        return;
    };

    let window = ApplicationWindow::builder()
        .application(&app)
        .title("Download Models")
        .transient_for(parent)
        .modal(true)
        .default_width(790)
        .default_height(820)
        .build();
    window.set_hide_on_close(true);

    let root = GtkBox::new(Orientation::Vertical, 12);
    root.set_margin_top(16);
    root.set_margin_bottom(16);
    root.set_margin_start(16);
    root.set_margin_end(16);

    let title = Label::new(Some("Select one or more models to download."));
    title.set_halign(Align::Start);
    title.set_xalign(0.0);
    title.add_css_class("title-2");

    let subtitle = Label::new(Some(
        "Larger models require more disk space and processing time.",
    ));
    subtitle.set_halign(Align::Start);
    subtitle.set_xalign(0.0);
    subtitle.set_wrap(true);

    let search_row = GtkBox::new(Orientation::Horizontal, 12);
    let search_entry = Entry::new();
    search_entry.set_hexpand(true);
    search_entry.set_placeholder_text(Some("Search models..."));
    let global_select_all = CheckButton::with_label("Select all");
    global_select_all.set_halign(Align::End);
    global_select_all.set_valign(Align::Center);
    search_row.append(&search_entry);
    search_row.append(&global_select_all);

    let header_row = GtkBox::new(Orientation::Horizontal, 12);
    header_row.add_css_class("whisper-download-header");
    header_row.set_margin_start(0);
    header_row.set_margin_end(0);
    header_row.set_margin_top(0);
    header_row.set_margin_bottom(0);
    let header_model = Label::new(Some("Model"));
    header_model.set_hexpand(false);
    header_model.set_xalign(0.0);
    header_model.set_size_request(DOWNLOAD_MODEL_COLUMN_WIDTH, -1);
    let header_quant = Label::new(Some("Quantization"));
    header_quant.set_xalign(0.0);
    header_quant.set_size_request(DOWNLOAD_QUANT_COLUMN_WIDTH, -1);
    let header_size = Label::new(Some("Size"));
    header_size.set_xalign(1.0);
    header_size.set_size_request(DOWNLOAD_SIZE_COLUMN_WIDTH, -1);
    let header_status = Label::new(Some("Status"));
    header_status.set_xalign(0.0);
    header_status.set_size_request(DOWNLOAD_STATUS_COLUMN_WIDTH, -1);
    header_row.append(&download_chevron_spacer());
    header_row.append(&header_model);
    header_row.append(&header_quant);
    header_row.append(&header_size);
    header_row.append(&header_status);

    let scroller = ScrolledWindow::new();
    scroller.set_hexpand(true);
    scroller.set_vexpand(true);
    scroller.set_min_content_height(560);

    let list_column = GtkBox::new(Orientation::Vertical, 0);
    list_column.set_hexpand(true);

    let rows = Rc::new(RefCell::new(Vec::<ModelRowWidgets>::new()));
    let groups = Rc::new(RefCell::new(Vec::<GroupWidgets>::new()));
    let download_selection_guard = Rc::new(RefCell::new(false));
    let download_destination = whisper_download_cache_dir();

    let mut grouped_rows: HashMap<&'static str, Vec<usize>> = HashMap::new();
    for model in list_downloadable_whisper_models().iter().copied() {
        grouped_rows
            .entry(model.family)
            .or_default()
            .push(rows.borrow().len());
        rows.borrow_mut()
            .push(build_downloadable_model_row(model, &download_destination));
    }

    let family_order = [
        "Tiny",
        "Base",
        "Small",
        "Medium",
        "Large-v1",
        "Large-v2",
        "Large-v3",
        "Large-v3-turbo",
    ];

    for family in family_order {
        let row_indices = grouped_rows.remove(family).unwrap_or_default();
        if row_indices.is_empty() {
            continue;
        }

        let expanded = Rc::new(RefCell::new(
            family != "Medium"
                && family != "Large-v1"
                && family != "Large-v2"
                && family != "Large-v3"
                && family != "Large-v3-turbo",
        ));
        let children = GtkBox::new(Orientation::Vertical, 0);
        children.set_hexpand(true);
        for index in &row_indices {
            if let Some(row) = rows.borrow().get(*index) {
                children.append(&row.row);
            }
        }
        children.set_visible(*expanded.borrow());

        let group = build_download_group_header(family, expanded, &children, row_indices.clone());
        list_column.append(&group.header);
        list_column.append(&children);
        groups.borrow_mut().push(group);
    }

    scroller.set_child(Some(&list_column));

    let table_box = GtkBox::new(Orientation::Vertical, 0);
    table_box.append(&header_row);
    table_box.append(&scroller);

    let table_frame = Frame::new(None);
    table_frame.add_css_class("whisper-download-table");
    table_frame.set_hexpand(true);
    table_frame.set_vexpand(true);
    table_frame.set_child(Some(&table_box));

    let action_row = GtkBox::new(Orientation::Horizontal, 12);
    let close_button = Button::with_label("Close");
    close_button.set_size_request(92, -1);
    let download_button = Button::with_label("Download Selected (0)");
    download_button.set_sensitive(false);
    download_button.set_size_request(210, -1);
    let action_spacer = GtkBox::new(Orientation::Horizontal, 0);
    action_spacer.set_hexpand(true);
    action_row.append(&close_button);
    action_row.append(&action_spacer);
    action_row.append(&download_button);

    root.append(&title);
    root.append(&subtitle);
    root.append(&search_row);
    root.append(&table_frame);
    root.append(&action_row);

    window.set_child(Some(&root));

    let state = Rc::new(DownloadDialogState {
        rows: rows.borrow().clone(),
        groups: groups.borrow().clone(),
        global_select_all: global_select_all.clone(),
        download_button: download_button.clone(),
        search_entry: search_entry.clone(),
        selection_guard: download_selection_guard.clone(),
        queued_model_ids: Rc::new(RefCell::new(HashSet::new())),
    });
    state.update_group_checks();
    state.update_action_labels();
    state.apply_filter();

    for row in &state.rows {
        let state = state.clone();
        row.check_button.connect_toggled(move |_| {
            if *state.selection_guard.borrow() {
                return;
            }
            state.update_from_row_toggle();
        });
    }

    {
        let state = state.clone();
        search_entry.connect_changed(move |_| {
            state.apply_filter();
        });
    }

    {
        let state = state.clone();
        global_select_all.connect_toggled(move |check| {
            if *state.selection_guard.borrow() {
                return;
            }
            state.select_all_models(check.is_active());
        });
    }

    for group in &state.groups {
        let state = state.clone();
        let group = group.clone();
        group.select_all.connect_toggled(move |check| {
            if *state.selection_guard.borrow() {
                return;
            }

            let selected = check.is_active();
            state.with_selection_guard(|| {
                for index in &group.row_indices {
                    if let Some(row) = state.rows.get(*index) {
                        row.check_button.set_active(selected);
                    }
                }
            });
            state.update_group_checks();
            state.update_action_labels();
        });
    }

    let model_dropdown = model_dropdown.clone();
    let model_entries = model_entries.clone();
    let config_for_refresh = config.clone();
    let invalid_models_for_refresh = invalid_models.clone();
    let warning_icons_for_refresh = warning_icons.clone();
    let selection_guard_for_refresh = selection_guard.clone();
    let refresh_models = Rc::new(move || {
        let model_paths = list_whisper_models();
        refresh_model_dropdown_items(
            &config_for_refresh,
            model_paths,
            &model_entries,
            &model_dropdown,
            &invalid_models_for_refresh,
            &warning_icons_for_refresh,
            &selection_guard_for_refresh,
        );
    });

    let download_state = state.clone();
    let refresh_models_for_download = refresh_models.clone();
    let row_map = {
        let mut map = HashMap::new();
        for row in &download_state.rows {
            map.insert(row.model.model_id.to_string(), row.clone());
        }
        Rc::new(map)
    };

    download_button.connect_clicked(move |_| {
        let selected_models = download_state.eligible_selected_models();
        if selected_models.is_empty() {
            return;
        }

        let destination = whisper_download_cache_dir();
        let (sender, receiver) = std::sync::mpsc::channel::<WhisperModelDownloadEvent>();
        let pending = Rc::new(RefCell::new(selected_models.len()));
        let pending_for_events = pending.clone();
        let pending_for_queue_pulse = pending.clone();
        let refresh_models = refresh_models_for_download.clone();
        let row_map = row_map.clone();
        let queued_model_ids = download_state.queued_model_ids.clone();
        queued_model_ids.borrow_mut().clear();

        download_whisper_models(selected_models, destination, 4, sender);

        gtk::glib::timeout_add_local(std::time::Duration::from_millis(50), move || {
            while let Ok(event) = receiver.try_recv() {
                match event {
                    WhisperModelDownloadEvent::Queued { model_id } => {
                        if let Some(row) = row_map.get(&model_id) {
                            queued_model_ids.borrow_mut().insert(row.model.model_id);
                            row.set_status(WhisperModelDownloadStatus::Queued);
                        }
                    }
                    WhisperModelDownloadEvent::Started {
                        model_id,
                        total_bytes,
                    } => {
                        if let Some(row) = row_map.get(&model_id) {
                            queued_model_ids.borrow_mut().remove(row.model.model_id);
                            row.set_status(WhisperModelDownloadStatus::Downloading {
                                downloaded_bytes: 0,
                                total_bytes,
                            });
                        }
                    }
                    WhisperModelDownloadEvent::Progress {
                        model_id,
                        downloaded_bytes,
                        total_bytes,
                    } => {
                        if let Some(row) = row_map.get(&model_id) {
                            row.set_status(WhisperModelDownloadStatus::Downloading {
                                downloaded_bytes,
                                total_bytes,
                            });
                        }
                    }
                    WhisperModelDownloadEvent::Downloaded { model_id, .. } => {
                        if let Some(row) = row_map.get(&model_id) {
                            queued_model_ids.borrow_mut().remove(row.model.model_id);
                            row.set_status(WhisperModelDownloadStatus::Downloaded);
                        }
                        let mut remaining = pending_for_events.borrow_mut();
                        let next = (*remaining).saturating_sub(1);
                        *remaining = next;
                        if *remaining == 0 {
                            refresh_models();
                        }
                    }
                    WhisperModelDownloadEvent::Skipped { model_id, .. } => {
                        if let Some(row) = row_map.get(&model_id) {
                            queued_model_ids.borrow_mut().remove(row.model.model_id);
                            row.set_status(WhisperModelDownloadStatus::Skipped);
                        }
                        let mut remaining = pending_for_events.borrow_mut();
                        let next = (*remaining).saturating_sub(1);
                        *remaining = next;
                        if *remaining == 0 {
                            refresh_models();
                        }
                    }
                    WhisperModelDownloadEvent::Failed { model_id, error } => {
                        if let Some(row) = row_map.get(&model_id) {
                            queued_model_ids.borrow_mut().remove(row.model.model_id);
                            row.set_status(WhisperModelDownloadStatus::Failed(error));
                        }
                        let mut remaining = pending_for_events.borrow_mut();
                        let next = (*remaining).saturating_sub(1);
                        *remaining = next;
                        if *remaining == 0 {
                            refresh_models();
                        }
                    }
                }
            }

            if *pending_for_events.borrow() == 0 {
                gtk::glib::ControlFlow::Break
            } else {
                gtk::glib::ControlFlow::Continue
            }
        });

        let rows = download_state.rows.clone();
        let queued_model_ids = download_state.queued_model_ids.clone();
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(120), move || {
            let queued = queued_model_ids.borrow();
            if queued.is_empty() && *pending_for_queue_pulse.borrow() == 0 {
                return gtk::glib::ControlFlow::Break;
            }

            for row in &rows {
                if queued.contains(row.model.model_id) {
                    row.queued_progress.pulse();
                }
            }

            gtk::glib::ControlFlow::Continue
        });
    });

    close_button.connect_clicked({
        let window = window.clone();
        move |_| {
            window.close();
        }
    });

    window.present();
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

fn model_dropdown_with_validation(
    model: &StringList,
    ellipsize: EllipsizeMode,
    invalid_models: Arc<Mutex<HashMap<String, bool>>>,
    warning_icons: ModelWarningIcons,
) -> DropDown {
    let factory = SignalListItemFactory::new();
    let invalid_models_for_bind = invalid_models.clone();
    let warning_icons_for_bind = warning_icons.clone();
    factory.connect_setup(move |_, item| {
        let row = GtkBox::new(Orientation::Horizontal, 8);
        let label = Label::new(None);
        label.set_hexpand(true);
        label.set_xalign(0.0);
        label.set_ellipsize(ellipsize);

        let warning_icon = Image::from_icon_name("dialog-warning-symbolic");
        warning_icon.set_pixel_size(16);
        warning_icon.set_visible(false);

        row.append(&label);
        row.append(&warning_icon);

        let list_item = item
            .downcast_ref::<gtk::ListItem>()
            .expect("drop-down setup item must be a GtkListItem");
        list_item.set_child(Some(&row));
    });
    factory.connect_bind(move |_, item| {
        let list_item = item
            .downcast_ref::<gtk::ListItem>()
            .expect("drop-down bind item must be a GtkListItem");
        let row = list_item
            .child()
            .and_then(|child| child.downcast::<GtkBox>().ok())
            .expect("drop-down list item child must be a GtkBox");
        let label = row
            .first_child()
            .and_then(|child| child.downcast::<Label>().ok())
            .expect("drop-down list item label child must be a GtkLabel");
        let warning_icon = label
            .next_sibling()
            .and_then(|child| child.downcast::<Image>().ok())
            .expect("drop-down list item warning child must be a GtkImage");
        let text = list_item
            .item()
            .and_then(|obj| obj.downcast::<StringObject>().ok())
            .map(|obj| obj.string())
            .unwrap_or_default();
        let text = text.to_string();

        label.set_label(&text);
        let is_invalid = invalid_models_for_bind
            .lock()
            .unwrap()
            .get(&text)
            .copied()
            .unwrap_or(false);
        warning_icon.set_visible(is_invalid);

        let mut warning_icons = warning_icons_for_bind.lock().unwrap();
        for icons in warning_icons.values_mut() {
            icons.retain(|weak_icon| {
                if let Some(icon) = weak_icon.upgrade() {
                    icon.as_ptr() != warning_icon.as_ptr()
                } else {
                    false
                }
            });
        }
        warning_icons
            .entry(text)
            .or_default()
            .push(warning_icon.downgrade().into());
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

        .whisper-download-table {
            background: #ffffff;
            border: 1px solid alpha(@theme_fg_color, 0.18);
            border-radius: 6px;
        }

        .whisper-download-header {
			padding: 5px 0px;
            background: #ffffff;
            border-bottom: 1px solid alpha(@theme_fg_color, 0.12);
        }

        .whisper-download-group {
            background-color: #f7f7f7;
            border-bottom: 1px solid alpha(@theme_fg_color, 0.10);
        }

        .whisper-download-row {
			padding: 5px 0px;
            background: #ffffff;
            border-bottom: 1px solid alpha(@theme_fg_color, 0.08);
        }

        "#,
    );

    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn refresh_model_warning_icons(
    model_path: &str,
    is_invalid: bool,
    invalid_models: &Arc<Mutex<HashMap<String, bool>>>,
    warning_icons: &ModelWarningIcons,
) {
    invalid_models
        .lock()
        .unwrap()
        .insert(model_path.to_string(), is_invalid);

    let mut warning_icons = warning_icons.lock().unwrap();
    if let Some(icons) = warning_icons.get_mut(model_path) {
        icons.retain(|weak_icon| {
            if let Some(icon) = weak_icon.upgrade() {
                icon.set_visible(is_invalid);
                true
            } else {
                false
            }
        });
    }
}

fn validate_whisper_models_on_load(
    model_paths: Vec<std::path::PathBuf>,
    invalid_models: Arc<Mutex<HashMap<String, bool>>>,
    warning_icons: ModelWarningIcons,
) {
    std::thread::spawn(move || {
        for model_path in model_paths {
            let model_path = model_path.to_string_lossy().to_string();
            let is_invalid =
                validate_whisper_model_path_in_subprocess(Path::new(&model_path)).is_err();
            let invalid_models = invalid_models.clone();
            let warning_icons = warning_icons.clone();
            gtk::glib::MainContext::default().invoke(move || {
                refresh_model_warning_icons(
                    &model_path,
                    is_invalid,
                    &invalid_models,
                    &warning_icons,
                );
            });
        }
    });
}

#[allow(clippy::too_many_arguments)]
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
        let result = validate_whisper_model_path_in_subprocess(Path::new(&model_path))
            .map_err(|err| err.to_string());
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
            levels: VecDeque::from(vec![0.0; OVERLAY_BAR_COUNT]),
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
        .focus_on_click(false)
        .modal(false)
        .default_width(200)
        .default_height(40)
        .build();
    window.set_hide_on_close(true);
    window.set_can_target(false);
    window.add_css_class("whisper-recording-overlay");
    let state = Rc::new(RefCell::new(WaveformState::default()));
    let area = DrawingArea::new();
    area.add_css_class("whisper-recording-overlay");
    area.set_content_width(200);
    area.set_content_height(40);
    area.set_hexpand(false);
    area.set_vexpand(false);
    area.set_can_target(false);
    area.set_focus_on_click(false);

    let draw_state = state.clone();
    area.set_draw_func(move |_, cr, width, height| {
        draw_overlay(cr, width, height, &draw_state.borrow());
    });

    window.set_child(Some(&area));
    window.connect_realize(|window| {
        prepare_overlay_window(window);
    });
    prepare_overlay_window(&window);

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

    let mic_height = OVERLAY_MIC_ICON_HEIGHT as f64;
    let mic_width = overlay_mic_icon_width_for_height(mic_height);
    draw_mic_icon(
        cr,
        OVERLAY_MIC_ICON_X,
        OVERLAY_MIC_ICON_Y,
        mic_width,
        mic_height,
    );

    let waveform_x = OVERLAY_MIC_ICON_X + mic_width + OVERLAY_WAVEFORM_X_PADDING;
    let waveform_width = width - waveform_x - OVERLAY_WAVEFORM_RIGHT_PADDING;
    draw_waveform(cr, state, waveform_x, 8.0, waveform_width, height - 16.0);
}

fn draw_mic_icon(cr: &gtk::cairo::Context, x: f64, y: f64, width: f64, height: f64) {
    if draw_overlay_svg_mic_icon(cr, x, y, width, height) {
        return;
    }

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

fn draw_overlay_svg_mic_icon(
    cr: &gtk::cairo::Context,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
) -> bool {
    let Some(icon_surface) = overlay_mic_icon_surface() else {
        return false;
    };

    let icon_width = icon_surface.width() as f64;
    let icon_height = icon_surface.height() as f64;
    if icon_width <= 0.0 || icon_height <= 0.0 {
        return false;
    }

    let scale = height / icon_height;
    let scaled_width = icon_width * scale;
    let scaled_height = icon_height * scale;
    let offset_x = x + (width - scaled_width) / 2.0;
    let offset_y = y + (height - scaled_height) / 2.0;

    let _ = cr.save();
    cr.translate(offset_x, offset_y);
    cr.scale(scale, scale);
    cr.set_source_rgba(0.93, 0.96, 0.97, 0.94);
    let _ = cr.mask_surface(&icon_surface, 0.0, 0.0);
    let _ = cr.restore();
    true
}

fn overlay_mic_icon_width_for_height(height: f64) -> f64 {
    height * OVERLAY_MIC_ICON_ASPECT_RATIO
}

fn overlay_mic_icon_surface() -> Option<gtk::cairo::ImageSurface> {
    thread_local! {
        static OVERLAY_MIC_SURFACE: RefCell<Option<gtk::cairo::ImageSurface>> = const { RefCell::new(None) };
    }

    OVERLAY_MIC_SURFACE.with(|cell| {
        if cell.borrow().is_none() {
            *cell.borrow_mut() = load_overlay_mic_icon_surface().ok();
        }
        cell.borrow().clone()
    })
}

fn load_overlay_mic_icon_surface() -> Result<gtk::cairo::ImageSurface, glib::Error> {
    let loader = gtk::gdk_pixbuf::PixbufLoader::with_type("svg")?;
    loader.write(
        std::fs::read("dist/icon_mic.svg")
            .map_err(|err| glib::Error::new(glib::FileError::Failed, &err.to_string()))?
            .as_slice(),
    )?;
    loader.close()?;
    let pixbuf = loader.pixbuf().ok_or_else(|| {
        glib::Error::new(
            glib::FileError::Failed,
            "failed to rasterize dist/icon_mic.svg for overlay",
        )
    })?;

    let surface = gtk::cairo::ImageSurface::create(
        gtk::cairo::Format::ARgb32,
        pixbuf.width(),
        pixbuf.height(),
    )
    .map_err(|err| glib::Error::new(glib::FileError::Failed, &err.to_string()))?;
    let icon_cr = gtk::cairo::Context::new(&surface)
        .map_err(|err| glib::Error::new(glib::FileError::Failed, &err.to_string()))?;
    icon_cr.set_source_pixbuf(&pixbuf, 0.0, 0.0);
    icon_cr
        .paint()
        .map_err(|err| glib::Error::new(glib::FileError::Failed, &err.to_string()))?;
    Ok(surface)
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
    let model_validation_state = Arc::new(Mutex::new(HashMap::new()));
    let model_warning_icons = Arc::new(Mutex::new(HashMap::new()));
    let model_dropdown = model_dropdown_with_validation(
        &model_entries,
        EllipsizeMode::Start,
        model_validation_state.clone(),
        model_warning_icons.clone(),
    );
    model_dropdown.set_halign(Align::Fill);
    let model_browse_button = Button::with_label("Browse...");
    model_browse_button.set_hexpand(true);
    let model_download_button = Button::with_label("Download Models");
    model_download_button.set_hexpand(true);
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

    model_grid.attach(&model_dropdown, 0, 0, 2, 1);
    model_grid.attach(&model_browse_button, 0, 1, 1, 1);
    model_grid.attach(&model_download_button, 1, 1, 1, 1);
    model_body.append(&model_grid);

    refresh_model_dropdown_items(
        &config,
        model_paths.clone(),
        &model_entries,
        &model_dropdown,
        &model_validation_state,
        &model_warning_icons,
        &model_selection_guard,
    );

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
            model_selection_guard_for_combo.clone(),
            id,
            false,
            current_model_for_restore,
        );
    });

    let open_model_picker_for_button = open_model_picker.clone();
    model_browse_button.connect_clicked(move |_| {
        open_model_picker_for_button();
    });

    let config_for_download = config.clone();
    let model_dropdown_for_download = model_dropdown.clone();
    let model_entries_for_download = model_entries.clone();
    let invalid_models_for_download = model_validation_state.clone();
    let warning_icons_for_download = model_warning_icons.clone();
    let selection_guard_for_download = model_selection_guard.clone();
    let window_for_download = window.clone();
    model_download_button.connect_clicked(move |_| {
        open_model_download_window(
            &window_for_download,
            config_for_download.clone(),
            model_dropdown_for_download.clone(),
            model_entries_for_download.clone(),
            invalid_models_for_download.clone(),
            warning_icons_for_download.clone(),
            selection_guard_for_download.clone(),
        );
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
