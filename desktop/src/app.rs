//! egui front-end: connect, camera/resolution selection, live preview,
//! loopback control and the status bar.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;
use parking_lot::Mutex;

use crate::loopback::{self, DeviceInfo, LoopCmd, LoopStatus, NO_DEVICES_HINT};
use crate::protocol::{self, Capabilities};
use crate::stream::{
    fetch_capabilities, ConnState, Frame, Sink, StreamCmd, StreamStatus, StreamStats,
};

/// Capabilities fetch results are delivered to the UI thread over this.
type CapsReply = Result<Capabilities, String>;

/// How often the preview texture may be re-uploaded (~30 fps).
const TEXTURE_MIN_INTERVAL: Duration = Duration::from_millis(33);

/// Rolling FPS estimator shared by the three counters in the status bar.
struct FpsMeter {
    last_count: u64,
    last_at: Instant,
    fps: f64,
}

impl FpsMeter {
    fn new() -> Self {
        FpsMeter {
            last_count: 0,
            last_at: Instant::now(),
            fps: 0.0,
        }
    }

    /// Feed the cumulative counter; returns the rate over the last window.
    fn tick(&mut self, total: u64) -> f64 {
        let now = Instant::now();
        let dt = now.duration_since(self.last_at).as_secs_f64();
        if dt >= 0.5 {
            self.fps = (total.saturating_sub(self.last_count)) as f64 / dt;
            self.last_count = total;
            self.last_at = now;
        }
        self.fps
    }
}

pub struct App {
    // Connect section
    addr: String,
    caps: Option<Capabilities>,
    caps_error: Option<String>,
    fetching_caps: bool,
    connect_rx: Option<Receiver<CapsReply>>,

    // Stream selection
    sel_camera: usize,
    sel_resolution: usize,

    // Stream plumbing
    stream_tx: Sender<StreamCmd>,
    stream_status: Arc<Mutex<StreamStatus>>,
    stream_stats: Arc<StreamStats>,
    frame_rx: Receiver<Arc<Frame>>,
    texture: Option<egui::TextureHandle>,
    last_texture_upload: Option<Instant>,
    last_uploaded_frame: Option<u64>,

    // Loopback
    devices: Vec<DeviceInfo>,
    scan_error: Option<String>,
    sel_device: usize,
    loop_tx: Sender<LoopCmd>,
    loop_status: Arc<Mutex<LoopStatus>>,
    loop_written: Arc<AtomicU64>,

    // Status bar meters
    delivered_fps: FpsMeter,
    decode_fps: FpsMeter,
    loop_fps: FpsMeter,
}

impl App {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        // Frames: bounded(1) latest-wins channels; the producer evicts stale
        // frames inside Sink, the consumer keeps only the newest.
        let (ui_tx, ui_rx) = crossbeam_channel::bounded::<Arc<Frame>>(1);
        let (lb_tx, lb_rx) = crossbeam_channel::bounded::<Arc<Frame>>(1);
        let sink_ui = Sink::new(ui_tx, ui_rx.clone());
        let sink_loop = Sink::new(lb_tx, lb_rx.clone());

        let stream_status = Arc::new(Mutex::new(StreamStatus::default()));
        let stream_stats = Arc::new(StreamStats::default());
        let loop_status = Arc::new(Mutex::new(LoopStatus::default()));
        let loop_written = Arc::new(AtomicU64::new(0));

        // Stream reader thread: owns the HTTP socket read loop.
        let (stream_tx, stream_cmd_rx) = crossbeam_channel::unbounded::<StreamCmd>();
        {
            let status = stream_status.clone();
            let stats = stream_stats.clone();
            std::thread::Builder::new()
                .name("opencamo-stream".into())
                .spawn(move || {
                    crate::stream::run_stream_thread(stream_cmd_rx, [sink_ui, sink_loop], status, stats)
                })
                .expect("failed to spawn stream thread");
        }

        // Loopback writer thread.
        let (loop_tx, loop_cmd_rx) = crossbeam_channel::unbounded::<LoopCmd>();
        {
            let status = loop_status.clone();
            let written = loop_written.clone();
            std::thread::Builder::new()
                .name("opencamo-loopback".into())
                .spawn(move || {
                    loopback::run_loopback_thread(loop_cmd_rx, lb_rx, status, written)
                })
                .expect("failed to spawn loopback thread");
        }

        let devices = loopback::scan_devices();
        let scan_error = if devices.is_empty() {
            Some(NO_DEVICES_HINT.to_string())
        } else {
            None
        };

        App {
            addr: "192.168.1.100:47777".to_string(),
            caps: None,
            caps_error: None,
            fetching_caps: false,
            connect_rx: None,
            sel_camera: 0,
            sel_resolution: 0,
            stream_tx,
            stream_status,
            stream_stats,
            frame_rx: ui_rx,
            texture: None,
            last_texture_upload: None,
            last_uploaded_frame: None,
            devices,
            scan_error,
            sel_device: 0,
            loop_tx,
            loop_status,
            loop_written,
            delivered_fps: FpsMeter::new(),
            decode_fps: FpsMeter::new(),
            loop_fps: FpsMeter::new(),
        }
    }

    // ---------------------------------------------------------------- connect

    fn start_connect(&mut self) {
        if self.fetching_caps {
            return;
        }
        let base = protocol::normalize_base(&self.addr);
        if base.is_empty() {
            self.caps_error = Some("Enter the phone's IP address".into());
            return;
        }
        self.caps_error = None;
        self.fetching_caps = true;
        let (tx, rx) = crossbeam_channel::bounded::<CapsReply>(1);
        self.connect_rx = Some(rx);
        std::thread::Builder::new()
            .name("opencamo-caps".into())
            .spawn(move || {
                let result = fetch_capabilities(&base).map_err(|e| format!("{:#}", e));
                let _ = tx.send(result);
            })
            .expect("failed to spawn capabilities thread");
    }

    /// Poll the capabilities fetch result (non-blocking).
    fn poll_caps(&mut self) {
        let Some(rx) = &self.connect_rx else { return };
        let reply = match rx.try_recv() {
            Ok(r) => r,
            Err(crossbeam_channel::TryRecvError::Empty) => return,
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                self.connect_rx = None;
                self.fetching_caps = false;
                return;
            }
        };
        self.connect_rx = None;
        self.fetching_caps = false;
        match reply {
            Ok(caps) => {
                // Default selection = first advertised camera/resolution.
                self.sel_camera = 0;
                self.sel_resolution = 0;
                self.caps = Some(caps);
                self.apply_selection();
            }
            Err(e) => {
                self.caps_error = Some(e);
                self.caps = None;
            }
        }
    }

    /// (Re)request `/v1/stream` with the current camera/resolution selection.
    fn apply_selection(&mut self) {
        let Some(caps) = &self.caps else { return };
        let Some(cam) = caps.cameras.get(self.sel_camera) else {
            return;
        };
        let Some(res) = cam.resolutions.get(self.sel_resolution) else {
            return;
        };
        let base = protocol::normalize_base(&self.addr);
        let fps = res.preferred_fps();
        let url = protocol::stream_url(&base, &cam.id, res.width, res.height, fps);
        let _ = self.stream_tx.send(StreamCmd::Connect { url });
    }

    fn disconnect(&mut self) {
        let _ = self.stream_tx.send(StreamCmd::Disconnect);
    }

    // -------------------------------------------------------------- loopback

    fn rescan_devices(&mut self) {
        self.devices = loopback::scan_devices();
        self.scan_error = if self.devices.is_empty() {
            Some(NO_DEVICES_HINT.to_string())
        } else {
            None
        };
        if self.sel_device >= self.devices.len() {
            self.sel_device = 0;
        }
    }

    fn loopback_enable(&mut self) {
        if self.devices.is_empty() {
            self.rescan_devices();
            if self.devices.is_empty() {
                let mut s = self.loop_status.lock();
                s.error = Some(NO_DEVICES_HINT.to_string());
                return;
            }
        }
        let Some(dev) = self.devices.get(self.sel_device) else {
            return;
        };
        if !loopback::device_exists(&dev.path) {
            let msg = loopback::module_hint(&dev.path, &"device node vanished");
            self.loop_status.lock().error = Some(msg);
            self.rescan_devices();
            return;
        }
        let _ = self.loop_tx.send(LoopCmd::Enable { path: dev.path.clone() });
    }

    fn loopback_disable(&mut self) {
        let _ = self.loop_tx.send(LoopCmd::Disable);
    }

    // ---------------------------------------------------------------- frames

    /// Drain the preview channel, keeping only the newest frame, and upload
    /// it to the GPU texture at most ~30 times per second.
    fn poll_frames(&mut self, ctx: &egui::Context) {
        let mut latest: Option<Arc<Frame>> = None;
        loop {
            match self.frame_rx.try_recv() {
                Ok(f) => latest = Some(f),
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => break,
            }
        }
        let Some(frame) = latest else { return };
        if self.last_uploaded_frame == Some(frame.id) {
            return;
        }
        let now = Instant::now();
        if let Some(t) = self.last_texture_upload {
            if now.duration_since(t) < TEXTURE_MIN_INTERVAL {
                return;
            }
        }
        let image = egui::ColorImage::from_rgb(
            [frame.width as usize, frame.height as usize],
            &frame.rgb,
        );
        match &mut self.texture {
            Some(tex) => tex.set(image, egui::TextureOptions::LINEAR),
            None => {
                self.texture = Some(ctx.load_texture(
                    "opencamo-preview",
                    image,
                    egui::TextureOptions::LINEAR,
                ));
            }
        }
        self.last_texture_upload = Some(now);
        self.last_uploaded_frame = Some(frame.id);
    }

    // ---------------------------------------------------------------- layout

    fn ui_connect(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Phone address:");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.addr)
                    .desired_width(220.0)
                    .hint_text("192.168.1.100:47777"),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            let clicked = ui
                .add_enabled(!self.fetching_caps, egui::Button::new("Connect"))
                .clicked();
            if clicked || enter {
                self.start_connect();
            }
            if self.fetching_caps {
                ui.spinner();
                ui.label("Fetching capabilities…");
            }
        });

        if let Some(err) = &self.caps_error {
            ui.colored_label(egui::Color32::LIGHT_RED, err);
        }

        if let Some(caps) = self.caps.clone() {
            ui.horizontal(|ui| {
                ui.strong(&caps.device_name);
                ui.label(format!(
                    "{} {} · protocol {}",
                    caps.app, caps.version, caps.protocol
                ));
                if ui.button("Refresh caps").clicked() {
                    self.start_connect();
                }
            });
        }
    }

    fn ui_selection(&mut self, ui: &mut egui::Ui) {
        let Some(caps) = self.caps.clone() else { return };

        ui.horizontal(|ui| {
            ui.label("Camera:");
            let cam_labels: Vec<String> = caps
                .cameras
                .iter()
                .map(|c| {
                    if c.label.is_empty() {
                        c.id.clone()
                    } else {
                        format!("{} ({})", c.label, c.id)
                    }
                })
                .collect();
            let mut changed = false;
            egui::ComboBox::from_id_source("cameras")
                .selected_text(
                    cam_labels
                        .get(self.sel_camera)
                        .cloned()
                        .unwrap_or_else(|| "—".into()),
                )
                .show_ui(ui, |ui| {
                    for (i, label) in cam_labels.iter().enumerate() {
                        if ui
                            .selectable_value(&mut self.sel_camera, i, label)
                            .changed()
                        {
                            changed = true;
                        }
                    }
                });
            if changed {
                self.sel_resolution = 0;
            }
        });

        ui.horizontal(|ui| {
            ui.label("Resolution:");
            let res_labels: Vec<String> = caps
                .cameras
                .get(self.sel_camera)
                .map(|c| c.resolutions.iter().map(|r| r.label()).collect())
                .unwrap_or_default();
            egui::ComboBox::from_id_source("resolutions")
                .selected_text(
                    res_labels
                        .get(self.sel_resolution)
                        .cloned()
                        .unwrap_or_else(|| "—".into()),
                )
                .show_ui(ui, |ui| {
                    for (i, label) in res_labels.iter().enumerate() {
                        ui.selectable_value(&mut self.sel_resolution, i, label);
                    }
                });
            if ui.button("Apply").clicked() {
                self.apply_selection();
            }
            if ui.button("Disconnect").clicked() {
                self.disconnect();
            }
        });
    }

    fn ui_preview(&mut self, ui: &mut egui::Ui) {
        match &self.texture {
            None => {
                ui.centered_and_justified(|ui| {
                    ui.label("No frames yet — connect to a phone to start the preview.");
                });
            }
            Some(tex) => {
                let (fw, fh) = (tex.size()[0] as f32, tex.size()[1] as f32);
                let avail = ui.available_size();
                let scale = (avail.x / fw).min(avail.y / fh);
                let size = egui::vec2(fw * scale, fh * scale);
                ui.centered_and_justified(|ui| {
                    ui.image((tex.id(), size));
                });
            }
        }
    }

    fn ui_loopback(&mut self, ui: &mut egui::Ui) {
        ui.heading("Loopback output");
        ui.label("Write frames into a v4l2loopback device so browsers/apps can use it as a webcam.");

        if let Some(err) = &self.scan_error.clone() {
            ui.colored_label(egui::Color32::YELLOW, err);
        }

        ui.horizontal(|ui| {
            if self.devices.is_empty() {
                ui.label("No devices");
            } else {
                let labels: Vec<String> = self.devices.iter().map(|d| d.display()).collect();
                egui::ComboBox::from_id_source("loopback-dev")
                    .selected_text(
                        labels
                            .get(self.sel_device)
                            .cloned()
                            .unwrap_or_else(|| "—".into()),
                    )
                    .show_ui(ui, |ui| {
                        for (i, label) in labels.iter().enumerate() {
                            ui.selectable_value(&mut self.sel_device, i, label);
                        }
                    });
            }
            if ui.button("Rescan").clicked() {
                self.rescan_devices();
            }
        });

        let loop_status = self.loop_status.lock().clone();
        let enabled = loop_status.enabled;
        ui.horizontal(|ui| {
            let label = if enabled { "Disable" } else { "Enable" };
            if ui.button(label).clicked() {
                if enabled {
                    self.loopback_disable();
                } else {
                    self.loopback_enable();
                }
            }
            if enabled {
                ui.spinner();
                ui.label(format!("writing to {}", loop_status.device));
            }
        });

        if let Some(err) = &loop_status.error {
            let color = if loopback::is_missing_device_error(err) {
                egui::Color32::YELLOW
            } else {
                egui::Color32::LIGHT_RED
            };
            ui.colored_label(color, err);
        }
    }

    fn ui_status_bar(&mut self, ui: &mut egui::Ui) {
        let status = self.stream_status.lock().clone();
        let delivered = self.stream_stats.delivered.load(Ordering::Relaxed);
        let decoded = self.stream_stats.decoded.load(Ordering::Relaxed);
        let written = self.loop_written.load(Ordering::Relaxed);

        let d_fps = self.delivered_fps.tick(delivered);
        let dec_fps = self.decode_fps.tick(decoded);
        let l_fps = self.loop_fps.tick(written);

        ui.horizontal_wrapped(|ui| {
            let (color, text) = match &status.state {
                ConnState::Disconnected => (egui::Color32::GRAY, "Disconnected".to_string()),
                ConnState::Connecting => (egui::Color32::YELLOW, "Connecting…".to_string()),
                ConnState::Streaming => (
                    egui::Color32::LIGHT_GREEN,
                    format!("Connected {}", status.url),
                ),
                ConnState::Error(e) => (egui::Color32::LIGHT_RED, format!("Error: {}", e)),
            };
            ui.colored_label(color, text);
            ui.separator();
            ui.label(format!("delivered {:.1} fps", d_fps));
            ui.label(format!("decode {:.1} fps", dec_fps));
            ui.label(format!("loopback {:.1} fps", l_fps));
            ui.separator();
            ui.label(format!("{} frames decoded", decoded));
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_caps();
        self.poll_frames(ctx);

        egui::TopBottomPanel::top("connect").show(ctx, |ui| {
            ui.add_space(6.0);
            self.ui_connect(ui);
            ui.add_space(4.0);
            self.ui_selection(ui);
            ui.add_space(6.0);
        });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(4.0);
            self.ui_status_bar(ui);
            ui.add_space(4.0);
        });

        egui::SidePanel::right("loopback")
            .resizable(true)
            .default_width(340.0)
            .min_width(280.0)
            .show(ctx, |ui| {
                ui.add_space(8.0);
                self.ui_loopback(ui);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            self.ui_preview(ui);
        });

        // Keep repainting so channel polls run even with no input events.
        ctx.request_repaint_after(Duration::from_millis(20));
    }
}
