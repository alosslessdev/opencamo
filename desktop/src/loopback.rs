//! v4l2loopback output: scan `/dev/video*` for devices (card name via
//! VIDIOC_QUERYCAP), and a writer thread that feeds YUYV frames into the
//! selected device using the v4l crate.
//!
//! Format contract (matches the v4l2loopback examples): open the device,
//! `VIDIOC_S_FMT` with `V4L2_BUF_TYPE_VIDEO_OUTPUT` at YUYV + frame size,
//! then plain `write()` of one full frame per call — the driver treats each
//! `write()` as one frame and drops old ones (latest-wins at driver level).

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail};
use crossbeam_channel::{Receiver, RecvTimeoutError};
use parking_lot::Mutex;

use crate::stream::Frame;

/// A `/dev/video*` node with its QUERYCAP card name.
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub path: String,
    pub card: String,
}

impl DeviceInfo {
    /// Short name for the dropdown: "video0 — USB Camera".
    pub fn display(&self) -> String {
        let node = self.path.trim_start_matches("/dev/");
        format!("{} — {}", node, self.card)
    }
}

/// Scan /dev/video* and read each device's card name via VIDIOC_QUERYCAP.
/// Unreadable nodes are listed with a placeholder card name so they still
/// show up (permissions problems stay visible instead of silent).
pub fn scan_devices() -> Vec<DeviceInfo> {
    let mut indices: Vec<u32> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/dev") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(rest) = name.strip_prefix("video") {
                if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(n) = rest.parse::<u32>() {
                        indices.push(n);
                    }
                }
            }
        }
    }
    indices.sort_unstable();

    indices
        .into_iter()
        .map(|i| {
            let path = format!("/dev/video{}", i);
            let card = match v4l::Device::with_path(&path)
                .map_err(|e| e.to_string())
                .and_then(|d| d.query_caps().map_err(|e| e.to_string()))
            {
                Ok(caps) => caps.card,
                Err(e) => format!("<unreadable: {}>", e),
            };
            DeviceInfo { path, card }
        })
        .collect()
}

/// Commands sent from the UI to the loopback writer thread.
#[derive(Debug)]
pub enum LoopCmd {
    /// Start writing frames into `path`.
    Enable { path: String },
    /// Stop writing and release the device.
    Disable,
}

/// Shared, UI-visible loopback state.
#[derive(Debug, Clone)]
pub struct LoopStatus {
    pub enabled: bool,
    pub device: String,
    pub error: Option<String>,
}

impl Default for LoopStatus {
    fn default() -> Self {
        LoopStatus {
            enabled: false,
            device: String::new(),
            error: None,
        }
    }
}

/// The actionable hint shown when no loopback device exists at all.
pub const NO_DEVICES_HINT: &str =
    "No /dev/video* devices found — is v4l2loopback loaded? Try: \
     sudo modprobe v4l2loopback devices=1 card_label=opencamo exclusive_caps=1";

pub fn module_hint(path: &str, err: &dyn std::fmt::Display) -> String {
    format!(
        "cannot use {}: {} — if the device is missing, load the module: \
         sudo modprobe v4l2loopback devices=1 card_label=opencamo exclusive_caps=1",
        path, err
    )
}

/// An open loopback device locked to a YUYV frame size.
struct Session {
    dev: v4l::Device,
    width: u32,
    height: u32,
}

/// Open `path`, negotiate YUYV at (width, height), and verify the driver
/// accepted it. The driver may override our size when another consumer has
/// already fixed the format — that is reported as a clear error.
fn open_session(path: &str, width: u32, height: u32) -> anyhow::Result<Session> {
    if width % 2 != 0 || height == 0 {
        bail!("YUYV requires an even frame width (got {}x{}), cannot write to {}", width, height, path);
    }
    let dev = v4l::Device::with_path(path)
        .map_err(|e| anyhow!("{}", module_hint(path, &e)))?;
    let want = v4l::Format::new(width, height, v4l::FourCC::new(b"YUYV"));
    let actual = v4l::video::Output::set_format(&dev, &want)
        .map_err(|e| anyhow!("{}", module_hint(path, &e)))?;
    if actual.width != width || actual.height != height || actual.fourcc != v4l::FourCC::new(b"YUYV")
    {
        bail!(
            "{} is locked to {}x{} {:?} by another consumer (wanted {}x{} YUYV); \
             disconnect it or restart the stream at that size",
            path,
            actual.width,
            actual.height,
            String::from_utf8_lossy(&actual.fourcc.repr),
            width,
            height
        );
    }
    Ok(Session { dev, width, height })
}

/// Convert packed RGB8 to packed YUYV (BT.601 limited range).
/// `width` must be even; `rgb` must be `width * height * 3` bytes.
pub fn rgb_to_yuyv(rgb: &[u8], width: u32, height: u32, out: &mut Vec<u8>) {
    debug_assert_eq!(rgb.len(), (width * height * 3) as usize);
    debug_assert_eq!(width % 2, 0);
    out.clear();
    out.resize((width * height * 2) as usize, 0);

    let mut o = 0usize;
    for row in rgb.chunks_exact((width * 3) as usize) {
        for pair in row.chunks_exact(6) {
            let (r0, g0, b0) = (pair[0] as i32, pair[1] as i32, pair[2] as i32);
            let (r1, g1, b1) = (pair[3] as i32, pair[4] as i32, pair[5] as i32);

            let y0 = ((66 * r0 + 129 * g0 + 25 * b0 + 128) >> 8) + 16;
            let y1 = ((66 * r1 + 129 * g1 + 25 * b1 + 128) >> 8) + 16;

            let (r, g, b) = ((r0 + r1) / 2, (g0 + g1) / 2, (b0 + b1) / 2);
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;

            out[o] = y0.clamp(0, 255) as u8;
            out[o + 1] = u.clamp(0, 255) as u8;
            out[o + 2] = y1.clamp(0, 255) as u8;
            out[o + 3] = v.clamp(0, 255) as u8;
            o += 4;
        }
    }
}

/// Run the loopback writer thread until the command sender is dropped.
///
/// Consumes the same latest-frame source as the UI (a bounded(1),
/// producer-evicted channel), writes each new frame once into the selected
/// `/dev/video*` device, and reports errors into `status`.
pub fn run_loopback_thread(
    cmd_rx: Receiver<LoopCmd>,
    frame_rx: Receiver<Arc<Frame>>,
    status: Arc<Mutex<LoopStatus>>,
    written: Arc<AtomicU64>,
) {
    let mut target: Option<String> = None;
    let mut session: Option<Session> = None;
    let mut yuyv: Vec<u8> = Vec::new();

    loop {
        // Drain every pending command (latest wins), then, if idle, block
        // for the next one.
        loop {
            match cmd_rx.try_recv() {
                Ok(LoopCmd::Enable { path }) => {
                    target = Some(path.clone());
                    session = None;
                    let mut s = status.lock();
                    s.enabled = true;
                    s.device = path;
                    s.error = None;
                }
                Ok(LoopCmd::Disable) => {
                    target = None;
                    session = None;
                    let mut s = status.lock();
                    s.enabled = false;
                    s.error = None;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => return,
            }
        }

        if target.is_none() {
            // Idle: block until the next command (or shutdown).
            match cmd_rx.recv() {
                Ok(LoopCmd::Enable { path }) => {
                    target = Some(path.clone());
                    session = None;
                    let mut s = status.lock();
                    s.enabled = true;
                    s.device = path;
                    s.error = None;
                }
                Ok(LoopCmd::Disable) => {
                    // Already disabled; nothing to do.
                }
                Err(_) => return,
            }
        }

        // Still disabled (idle Disable) → loop back to the blocking recv.
        let Some(path) = target.clone() else { continue };

        // Enabled: wait for the next frame, re-checking commands while idle.
        let mut frame = match frame_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(f) => f,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => {
                let mut s = status.lock();
                s.enabled = false;
                s.error = Some("stream thread exited; loopback disabled".into());
                return;
            }
        };
        // Latest-wins: skip anything queued behind this frame.
        loop {
            match frame_rx.try_recv() {
                Ok(newer) => frame = newer,
                Err(_) => break,
            }
        }

        // (Re)negotiate the device format whenever the frame size changes.
        if session
            .as_ref()
            .map(|s| s.width != frame.width || s.height != frame.height)
            .unwrap_or(true)
        {
            match open_session(&path, frame.width, frame.height) {
                Ok(s) => session = Some(s),
                Err(e) => {
                    let mut st = status.lock();
                    st.enabled = false;
                    st.error = Some(format!("{:#}", e));
                    target = None;
                    continue;
                }
            }
        }
        let sess = session.as_mut().expect("session just opened");

        if frame.width != sess.width || frame.height != sess.height {
            // Cannot happen (size change triggers renegotiation above).
            continue;
        }
        if frame.width % 2 != 0 || frame.rgb.len() != (frame.width * frame.height * 3) as usize {
            let mut st = status.lock();
            st.enabled = false;
            st.error = Some(format!(
                "received a malformed frame ({}x{}, {} bytes)",
                frame.width,
                frame.height,
                frame.rgb.len()
            ));
            target = None;
            continue;
        }

        rgb_to_yuyv(&frame.rgb, frame.width, frame.height, &mut yuyv);

        // One write() == one frame at the driver; a short write would land a
        // truncated frame, so treat anything but a full write as an error.
        match sess.dev.write(&yuyv) {
            Ok(n) if n == yuyv.len() => {
                written.fetch_add(1, Ordering::Relaxed);
                let mut st = status.lock();
                st.error = None;
            }
            Ok(n) => {
                let mut st = status.lock();
                st.enabled = false;
                st.error = Some(format!(
                    "short write to {}: {} of {} bytes; loopback disabled",
                    path, n, yuyv.len()
                ));
                target = None;
                session = None;
            }
            Err(e) => {
                let mut st = status.lock();
                st.enabled = false;
                st.error = Some(format!("write to {} failed: {}", path, e));
                target = None;
                session = None;
            }
        }
    }
}

/// Convenience used by the UI: is the error a "module not loaded" situation?
pub fn is_missing_device_error(msg: &str) -> bool {
    msg.contains("modprobe v4l2loopback")
}

/// Probe that a device node exists (used to give an immediate error on
/// Enable when the node has vanished since the last scan).
pub fn device_exists(path: &str) -> bool {
    std::path::Path::new(path).exists()
}
