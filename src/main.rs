#![allow(clashing_extern_declarations)]

mod config;
mod native;
mod services;
#[cfg(test)]
mod tests;
mod ui;

use crate::config::AppConfig;
use crate::native::TrayIndicator;
use crate::services::{
    focused_monitor_geometry, overlay_position_for_monitor, run_output_mode,
    spawn_xev_hotkey_listener, transcribe, RecordingGeneration, RecordingSession, RecordingStop,
};
use crate::ui::{build_overlay, build_settings_window, OverlayMeter};
use gtk::prelude::*;
use gtk::{Application, ApplicationWindow};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub enum AppEvent {
    HotkeyPressed,
    HotkeyReleased,
    Error(String),
    TranscriptionFinished { sequence: u64, text: Option<String> },
}

pub struct AppController {
    pub application: Application,
    pub event_sender: Sender<AppEvent>,
    pub config: Arc<Mutex<AppConfig>>,
    pub settings_window: Option<ApplicationWindow>,
    pub overlay_window: ApplicationWindow,
    pub overlay_meter: OverlayMeter,
    pub tray: Option<TrayIndicator>,
    pub recording: Option<RecordingSession>,
    pub recording_generation: RecordingGeneration,
    pub max_duration_timer: Option<gtk::glib::SourceId>,
    pub overlay_tick_timer: Option<gtk::glib::SourceId>,
    pub self_weak: Option<Weak<RefCell<AppController>>>,
    pub next_transcription_sequence: u64,
    pub next_transcription_to_output: u64,
    pub pending_transcripts: BTreeMap<u64, Option<String>>,
}

impl AppController {
    fn new(
        application: Application,
        config: AppConfig,
        event_sender: Sender<AppEvent>,
    ) -> Rc<RefCell<Self>> {
        let config = Arc::new(Mutex::new(config));
        let overlay = build_overlay(&application);
        let controller = Rc::new(RefCell::new(Self {
            application,
            event_sender,
            config,
            settings_window: None,
            overlay_window: overlay.window,
            overlay_meter: overlay.meter,
            tray: None,
            recording: None,
            recording_generation: RecordingGeneration::default(),
            max_duration_timer: None,
            overlay_tick_timer: None,
            self_weak: None,
            next_transcription_sequence: 1,
            next_transcription_to_output: 1,
            pending_transcripts: BTreeMap::new(),
        }));

        controller.borrow_mut().self_weak = Some(Rc::downgrade(&controller));
        let tray = TrayIndicator::new(Rc::downgrade(&controller));
        controller.borrow_mut().tray = Some(tray);
        controller
    }

    fn cancel_max_duration_timer(&mut self) {
        if let Some(source) = self.max_duration_timer.take() {
            source.remove();
        }
    }

    fn cancel_overlay_tick_timer(&mut self) {
        if let Some(source) = self.overlay_tick_timer.take() {
            source.remove();
        }
    }

    fn set_recording_ui(&self, recording: bool) {
        if recording {
            self.overlay_window.show();
        } else {
            self.overlay_window.hide();
        }
        if let Some(tray) = self.tray.as_ref() {
            tray.set_recording(recording);
        }
    }

    fn start_recording(&mut self) {
        if self.recording.is_some() {
            return;
        }
        let cfg = self.config.lock().unwrap().clone();
        match RecordingSession::start(&cfg) {
            Ok(session) => {
                self.cancel_max_duration_timer();
                self.cancel_overlay_tick_timer();
                self.recording = Some(session);
                let generation = self.recording_generation.next();
                self.set_recording_ui(true);
                let window = self.overlay_window.clone();
                gtk::glib::timeout_add_local_once(
                    std::time::Duration::from_millis(40),
                    move || {
                        position_overlay_window(&window);
                    },
                );
                self.start_overlay_tick_timer();
                let max_secs = cfg.max_recording_secs;
                if let Some(weak) = self.self_weak.clone() {
                    let source_id = gtk::glib::timeout_add_local_once(
                        std::time::Duration::from_secs(max_secs as u64),
                        move || {
                            if let Some(controller) = weak.upgrade() {
                                let mut controller = controller.borrow_mut();
                                if controller
                                    .recording_generation
                                    .current()
                                    .is_some_and(|current| current == generation)
                                {
                                    controller.stop_recording();
                                }
                            }
                        },
                    );
                    self.max_duration_timer = Some(source_id);
                }
            }
            Err(err) => {
                self.cancel_max_duration_timer();
                self.cancel_overlay_tick_timer();
                self.recording = None;
                if let Some(generation) = self.recording_generation.current() {
                    let _ = self.recording_generation.stop_if_current(generation);
                }
                self.set_recording_ui(false);
                eprintln!("recording failed: {err}");
            }
        }
    }

    fn stop_recording(&mut self) {
        let Some(session) = self.recording.take() else {
            return;
        };
        self.cancel_max_duration_timer();
        self.cancel_overlay_tick_timer();
        if let Some(generation) = self.recording_generation.current() {
            let _ = self.recording_generation.stop_if_current(generation);
        }
        self.set_recording_ui(false);
        let sender = self.event_sender.clone();
        let sequence = self.next_transcription_sequence;
        self.next_transcription_sequence = self.next_transcription_sequence.saturating_add(1);
        std::thread::spawn(move || {
            let result = session.stop().and_then(|stop| match stop {
                RecordingStop::Captured(wav) => {
                    let transcript = transcribe(&wav);
                    if let Err(err) = std::fs::remove_file(&wav) {
                        eprintln!(
                            "failed to remove temporary recording {}: {err}",
                            wav.display()
                        );
                    }
                    match transcript {
                        Ok(text) => Ok(Some(text)),
                        Err(err) => Err(err),
                    }
                }
                RecordingStop::Discarded(stats) => {
                    eprintln!(
                        "recording discarded: duration={}ms speech={}ms",
                        stats.duration().as_millis(),
                        stats.speech_duration().as_millis()
                    );
                    Ok(None)
                }
            });
            match result {
                Ok(Some(text)) => {
                    let text = (!text.trim().is_empty()).then_some(text);
                    let _ = sender.send(AppEvent::TranscriptionFinished { sequence, text });
                }
                Ok(None) => {
                    let _ = sender.send(AppEvent::TranscriptionFinished {
                        sequence,
                        text: None,
                    });
                }
                Err(err) => {
                    let _ = sender.send(AppEvent::Error(format!("transcription failed: {err}")));
                    let _ = sender.send(AppEvent::TranscriptionFinished {
                        sequence,
                        text: None,
                    });
                }
            }
        });
    }

    fn finish_transcription(&mut self, sequence: u64, text: Option<String>) {
        self.pending_transcripts.insert(sequence, text);
        while let Some(text) = self
            .pending_transcripts
            .remove(&self.next_transcription_to_output)
        {
            self.next_transcription_to_output = self.next_transcription_to_output.saturating_add(1);
            let Some(text) = text else {
                continue;
            };
            let cfg = self.config.lock().unwrap().clone();
            if let Err(err) = run_output_mode(cfg.output_mode, &text) {
                eprintln!("output failed: {err}");
            }
        }
    }

    fn show_settings_window(&mut self) {
        if let Some(win) = self.settings_window.as_ref() {
            win.present();
            win.show();
            return;
        }
        let config = self.config.clone();
        let window = build_settings_window(&self.application, config);
        self.settings_window = Some(window.clone());
        window.present();
        window.show();
    }

    fn quit(&mut self) {
        self.cancel_max_duration_timer();
        self.cancel_overlay_tick_timer();
        self.set_recording_ui(false);
        if let Some(session) = self.recording.take() {
            std::thread::spawn(move || {
                if let Ok(RecordingStop::Captured(wav)) = session.stop() {
                    let _ = std::fs::remove_file(wav);
                }
            });
        }
        self.application.quit();
    }

    fn start_overlay_tick_timer(&mut self) {
        let Some(weak) = self.self_weak.clone() else {
            return;
        };
        let source_id =
            gtk::glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
                let Some(controller) = weak.upgrade() else {
                    return gtk::glib::ControlFlow::Break;
                };
                let controller = controller.borrow_mut();
                if controller.recording.is_none() {
                    return gtk::glib::ControlFlow::Break;
                }

                if let Some(session) = controller.recording.as_ref() {
                    let mut latest = None;
                    while let Some(level) = session.try_read_level() {
                        latest = Some(level);
                    }
                    if let Some(level) = latest {
                        controller.overlay_meter.set_level(level);
                    }
                }
                controller.overlay_meter.tick();
                gtk::glib::ControlFlow::Continue
            });
        self.overlay_tick_timer = Some(source_id);
    }
}

fn position_overlay_window(window: &ApplicationWindow) {
    let Some(monitor) = focused_monitor_geometry() else {
        return;
    };
    let (x, y) = overlay_position_for_monitor(monitor, 200, 40);
    let title = window.title().unwrap_or_else(|| "Whisper Recording".into());
    let _ = std::process::Command::new("xdotool")
        .args([
            "search",
            "--name",
            &title,
            "windowraise",
            "windowmove",
            &x.to_string(),
            &y.to_string(),
        ])
        .output();
}

fn main() {
    let app = Application::builder()
        .application_id("dev.whisper.gtk")
        .build();

    app.connect_activate(|app| {
        let config = AppConfig::load();
        let (sender, receiver) = std::sync::mpsc::channel();
        let controller = AppController::new(app.clone(), config, sender.clone());
        unsafe {
            app.set_data("app-controller", controller.clone());
        }
        let shared_config: Arc<Mutex<AppConfig>> = controller.borrow().config.clone();

        spawn_xev_hotkey_listener(shared_config.clone(), sender);

        let controller_weak: Weak<RefCell<AppController>> = Rc::downgrade(&controller);
        gtk::glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
            while let Ok(event) = receiver.try_recv() {
                if let Some(controller) = controller_weak.upgrade() {
                    let mut controller = controller.borrow_mut();
                    match event {
                        AppEvent::HotkeyPressed => controller.start_recording(),
                        AppEvent::HotkeyReleased => controller.stop_recording(),
                        AppEvent::Error(err) => eprintln!("{err}"),
                        AppEvent::TranscriptionFinished { sequence, text } => {
                            controller.finish_transcription(sequence, text)
                        }
                    }
                }
            }
            gtk::glib::ControlFlow::Continue
        });

        if !AppConfig::path().exists() {
            controller.borrow_mut().show_settings_window();
        }
        let _ = controller;
    });

    app.run();
}
