//! Shared wire-protocol types (see PROTOCOL.md).

use serde::Deserialize;

/// Default TCP port of the phone's HTTP server.
pub const DEFAULT_PORT: u16 = 47777;

#[derive(Debug, Clone, Deserialize)]
pub struct Capabilities {
    pub protocol: u32,
    pub app: String,
    pub version: String,
    pub device_name: String,
    pub cameras: Vec<Camera>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Camera {
    pub id: String,
    pub label: String,
    pub resolutions: Vec<Resolution>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
    pub fps: Vec<u32>,
}

impl Resolution {
    /// Human readable "WxH@fps1/fps2" label for the UI.
    pub fn label(&self) -> String {
        if self.fps.is_empty() {
            format!("{}x{}", self.width, self.height)
        } else {
            let fps: Vec<String> = self.fps.iter().map(|f| f.to_string()).collect();
            format!("{}x{}@{}", self.width, self.height, fps.join("/"))
        }
    }

    /// The fps we request for this resolution (highest advertised rate).
    pub fn preferred_fps(&self) -> u32 {
        self.fps.iter().copied().max().unwrap_or(30)
    }
}

/// Strip a leading `http://` / `https://` (case-insensitive), if present.
fn strip_scheme(input: &str) -> &str {
    for (prefix, len) in [("https://", 8usize), ("http://", 7)] {
        if input
            .get(..len)
            .is_some_and(|p| p.eq_ignore_ascii_case(prefix))
        {
            return &input[len..];
        }
    }
    input
}

/// Turn user input like "192.168.1.5", "192.168.1.5:47777" or
/// "http://192.168.1.5:47777/" into a canonical base URL "http://host:port".
/// Any path component is dropped — the result is always a bare origin.
pub fn normalize_base(input: &str) -> String {
    let s = strip_scheme(input.trim());
    // Keep only the authority (host[:port]); drop any path/query/fragment.
    let authority = s.split('/').next().unwrap_or("").trim_end_matches(':');
    if authority.is_empty() {
        return String::new();
    }
    // Append the default port only when the authority has none.
    let has_port = match authority.rfind(':') {
        Some(idx) => {
            let tail = &authority[idx + 1..];
            !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit())
        }
        None => false,
    };
    if has_port {
        format!("http://{}", authority)
    } else {
        format!("http://{}:{}", authority, DEFAULT_PORT)
    }
}

/// Build the `/v1/stream` URL for the given parameters.
pub fn stream_url(base: &str, camera: &str, width: u32, height: u32, fps: u32) -> String {
    format!(
        "{}/v1/stream?camera={}&width={}&height={}&fps={}",
        base.trim_end_matches('/'),
        camera,
        width,
        height,
        fps
    )
}

/// If `url` already points at `/v1/stream`, return it unchanged (query
/// parameters preserved); otherwise treat it as a base URL and append the
/// stream path with default params.
pub fn ensure_stream_url(url: &str) -> String {
    let trimmed = url.trim();
    // Strip scheme + authority to inspect just the path.
    let after_scheme = strip_scheme(trimmed);
    let path = after_scheme.split_once('/').map(|(_, p)| p).unwrap_or("");
    if path == "v1/stream" || path.starts_with("v1/stream?") {
        return trimmed.to_string();
    }
    let base = normalize_base(trimmed);
    format!("{}/v1/stream", base.trim_end_matches('/'))
}
