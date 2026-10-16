//! Background MJPEG stream reader: owns the HTTP socket read loop, parses
//! multipart parts (Content-Length based), decodes JPEG → RGB8 and publishes
//! latest-frame-wins to two bounded channels (UI preview + loopback writer).

use std::io::{BufRead, BufReader};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use parking_lot::Mutex;

use crate::protocol::Capabilities;

/// Multipart boundary mandated by PROTOCOL.md.
pub const BOUNDARY: &str = "frame";

/// Refuse absurd Content-Length values (guard against garbage streams).
const MAX_PART_BYTES: usize = 64 * 1024 * 1024;

/// One decoded video frame (RGB8).
#[derive(Debug)]
pub struct Frame {
    pub id: u64,
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

/// Connection state surfaced to the UI status bar.
#[derive(Debug, Clone)]
pub enum ConnState {
    Disconnected,
    Connecting,
    Streaming,
    Error(String),
}

impl std::fmt::Display for ConnState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnState::Disconnected => write!(f, "Disconnected"),
            ConnState::Connecting => write!(f, "Connecting…"),
            ConnState::Streaming => write!(f, "Streaming"),
            ConnState::Error(e) => write!(f, "Error: {}", e),
        }
    }
}

/// Shared stream status (guarded by a mutex, cheap to poll from the UI).
#[derive(Debug, Clone)]
pub struct StreamStatus {
    pub state: ConnState,
    pub url: String,
}

impl Default for StreamStatus {
    fn default() -> Self {
        StreamStatus {
            state: ConnState::Disconnected,
            url: String::new(),
        }
    }
}

/// Frame counters sampled by the UI to derive FPS.
#[derive(Default)]
pub struct StreamStats {
    /// JPEG parts received off the wire.
    pub delivered: AtomicU64,
    /// JPEG parts successfully decoded to RGB8.
    pub decoded: AtomicU64,
}

/// Commands the UI sends to the stream thread.
#[derive(Debug)]
pub enum StreamCmd {
    /// Open (or replace) the stream with this URL.
    Connect { url: String },
    /// Close the stream and go idle.
    Disconnect,
}

/// A latest-frame-wins sink over a bounded(1) channel.
///
/// The producer keeps a consumer-side `Receiver` clone so that when the
/// channel is full it can evict the stale frame and enqueue the newest one —
/// frames are dropped, never queued.
pub struct Sink {
    tx: Sender<Arc<Frame>>,
    evict: Receiver<Arc<Frame>>,
}

impl Sink {
    pub fn new(tx: Sender<Arc<Frame>>, evict: Receiver<Arc<Frame>>) -> Self {
        Sink { tx, evict }
    }

    pub fn push(&self, frame: Arc<Frame>) {
        let mut item = frame;
        for _ in 0..8 {
            match self.tx.try_send(item) {
                Ok(()) => return,
                Err(TrySendError::Full(f)) => {
                    let _ = self.evict.try_recv();
                    item = f;
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        // Contention beyond the retry budget: the frame stream keeps flowing,
        // so dropping this one frame is the correct latest-wins behaviour.
    }
}

/// Fetch `/v1/capabilities` from the phone.
pub fn fetch_capabilities(base: &str) -> anyhow::Result<Capabilities> {
    let agent: ureq::Agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(5))
        .timeout_write(Duration::from_secs(5))
        .build();
    let url = format!("{}/v1/capabilities", base.trim_end_matches('/'));
    let caps: Capabilities = match agent.get(&url).call() {
        Ok(resp) => resp
            .into_json::<Capabilities>()
            .context("invalid /v1/capabilities JSON")?,
        Err(e) => bail!("{}", describe_ureq_error(e)),
    };
    if caps.protocol != 1 {
        bail!(
            "phone speaks protocol {}, this client speaks protocol 1",
            caps.protocol
        );
    }
    if caps.cameras.is_empty() {
        bail!("phone reported no cameras");
    }
    Ok(caps)
}

/// Turn a ureq error (including non-2xx with `{"error": ...}` bodies) into a
/// human-readable message.
pub fn describe_ureq_error(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            #[derive(serde::Deserialize)]
            struct ErrBody {
                error: String,
            }
            let body = resp.into_string().unwrap_or_default();
            match serde_json::from_str::<ErrBody>(&body) {
                Ok(b) => format!("HTTP {}: {}", code, b.error),
                Err(_) => format!("HTTP {}", code),
            }
        }
        t => format!("{}", t),
    }
}

/// Read exactly one multipart part: `--frame` line, headers (with
/// `Content-Length`), `<n>` JPEG bytes, trailing CRLF.
///
/// Returns `Ok(None)` at a clean end of stream (`--frame--` or EOF).
pub fn read_part<R: BufRead>(reader: &mut R) -> anyhow::Result<Option<Vec<u8>>> {
    // Boundary line; tolerate an optional leading CRLF / preamble lines.
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).context("reading stream boundary")?;
        if n == 0 {
            return Ok(None);
        }
        let t = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if t.starts_with("--") {
            let t = t.trim_start_matches("--");
            if t.starts_with(BOUNDARY) {
                let rest = &t[BOUNDARY.len()..];
                if rest.starts_with("--") {
                    return Ok(None); // closing boundary
                }
                break;
            }
            // Unknown preamble line — skip it.
        }
    }

    // Part headers up to the blank line.
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).context("reading part headers")?;
        if n == 0 {
            bail!("stream ended inside part headers");
        }
        let t = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if t.is_empty() {
            break;
        }
        if let Some((name, value)) = t.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                let v = value
                    .trim()
                    .parse::<usize>()
                    .context("invalid Content-Length")?;
                if v > MAX_PART_BYTES {
                    bail!("Content-Length {} exceeds limit", v);
                }
                content_length = Some(v);
            }
        }
    }

    let len = content_length.ok_or_else(|| anyhow!("part missing Content-Length"))?;
    let mut buf = vec![0u8; len];
    reader
        .read_exact(&mut buf)
        .context("reading part body (stream truncated?)")?;
    // Trailing CRLF after the payload; tolerate EOF right after it.
    let mut crlf = [0u8; 2];
    let _ = reader.read_exact(&mut crlf);
    Ok(Some(buf))
}

/// Decode a JPEG buffer into an RGB8 `Frame`.
pub fn decode_jpeg(id: u64, jpeg: &[u8]) -> anyhow::Result<Frame> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg)
        .context("JPEG decode failed")?
        .to_rgb8();
    let (width, height) = img.dimensions();
    if width == 0 || height == 0 {
        bail!("decoded frame has zero dimension");
    }
    Ok(Frame {
        id,
        width,
        height,
        rgb: img.into_raw(),
    })
}

/// Run the stream thread until all command senders are dropped.
pub fn run_stream_thread(
    cmd_rx: Receiver<StreamCmd>,
    sinks: [Sink; 2],
    status: Arc<Mutex<StreamStatus>>,
    stats: Arc<StreamStats>,
) {
    let agent: ureq::Agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(15))
        .timeout_write(Duration::from_secs(10))
        .build();

    // A Connect that arrived while a session was ending continues immediately.
    let mut pending: Option<String> = None;
    loop {
        let url = match pending.take() {
            Some(u) => u,
            None => match cmd_rx.recv() {
                Ok(StreamCmd::Connect { url }) => url,
                Ok(StreamCmd::Disconnect) => {
                    set_status(&status, ConnState::Disconnected, "");
                    continue;
                }
                Err(_) => break, // UI gone
            },
        };

        match session(&agent, &url, &cmd_rx, &sinks, &status, &stats) {
            Ok(pending_url) => {
                pending = pending_url;
                if pending.is_none() {
                    set_status(&status, ConnState::Disconnected, "");
                }
            }
            Err(e) => {
                set_status(&status, ConnState::Error(format!("{:#}", e)), &url);
                // Wait for the next command (or shutdown).
                match cmd_rx.recv() {
                    Ok(StreamCmd::Connect { url }) => pending = Some(url),
                    Ok(StreamCmd::Disconnect) => {
                        set_status(&status, ConnState::Disconnected, "")
                    }
                    Err(_) => break,
                }
            }
        }
    }
}

fn set_status(status: &Arc<Mutex<StreamStatus>>, state: ConnState, url: &str) {
    let mut g = status.lock();
    g.state = state;
    g.url = url.to_string();
}

/// One connected streaming session. Returns `Ok(Some(url))` when a Connect
/// command arrived mid-session, `Ok(None)` on a clean stop, `Err` on failure.
fn session(
    agent: &ureq::Agent,
    url: &str,
    cmd_rx: &Receiver<StreamCmd>,
    sinks: &[Sink; 2],
    status: &Arc<Mutex<StreamStatus>>,
    stats: &Arc<StreamStats>,
) -> anyhow::Result<Option<String>> {
    set_status(status, ConnState::Connecting, url);

    let resp = agent
        .get(url)
        .call()
        .map_err(|e| anyhow!("{}", describe_ureq_error(e)))?;
    let content_type = resp.header("content-type").unwrap_or("").to_string();
    if !content_type.contains("multipart/x-mixed-replace") {
        bail!("expected multipart/x-mixed-replace, got {:?}", content_type);
    }
    if !content_type.contains("frame") {
        bail!("unexpected multipart boundary in {:?}", content_type);
    }

    set_status(status, ConnState::Streaming, url);
    let mut reader = BufReader::new(resp.into_reader());

    loop {
        // Drain commands between frames so Apply/Disconnect take effect fast.
        loop {
            match cmd_rx.try_recv() {
                Ok(StreamCmd::Disconnect) => return Ok(None),
                Ok(StreamCmd::Connect { url }) => return Ok(Some(url)),
                Err(TryRecvError::Empty) => break,
                // UI shut down — stop reading.
                Err(TryRecvError::Disconnected) => return Ok(None),
            }
        }

        let part = read_part(&mut reader)?.ok_or_else(|| anyhow!("stream closed by phone"))?;
        // Frame IDs come from the cumulative delivered counter so they stay
        // unique across reconnects (the UI dedupes on them).
        let id = stats.delivered.fetch_add(1, Ordering::Relaxed);

        let frame = decode_jpeg(id, &part)?;
        stats.decoded.fetch_add(1, Ordering::Relaxed);

        let frame = Arc::new(frame);
        sinks[0].push(frame.clone());
        sinks[1].push(frame);
    }
}
