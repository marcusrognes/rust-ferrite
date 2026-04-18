use serde::{Deserialize, Serialize};

/// Live host status, written by `ferrite-host` to `$XDG_RUNTIME_DIR/ferrite-status.json`
/// every ~500 ms and read by the UI for display.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub listen_addr: String,
    pub mode: String,
    pub clients: Vec<ClientStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientStatus {
    pub peer: String,
    pub width: u32,
    pub height: u32,
}

pub fn status_path() -> std::path::PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp"))
        .join("ferrite-status.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    /// Row-major, 8-bit-per-channel, R,G,B,A byte order per pixel. `data` length = width*height*4.
    Rgba8,
    /// Standard JPEG bytes. `data` is one encoded frame.
    Jpeg,
    /// H.264 Annex-B byte stream. `data` is an opaque chunk of the continuous
    /// encoded stream; multiple consecutive chunks must be fed to the decoder in
    /// order. NAL/AU boundaries may fall anywhere within or across chunks.
    H264,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum HostMessage {
    VideoFrame {
        format: PixelFormat,
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
    Ping,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PointerTool {
    Finger,
    Pen,
    Eraser,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TouchPoint {
    /// Stable per-finger identifier from the client's input layer (Android
    /// `getPointerId`). Host maps these to MT slots.
    pub id: u32,
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    /// First message after socket connect. `device_name` is shown in cosmic's
    /// display list (via EDID monitor-name descriptor in virtual mode) and
    /// suffixed onto the per-client pen device name. `width` / `height` are
    /// the client's screen pixels; in virtual mode the host creates an evdi
    /// monitor at exactly those dimensions.
    Hello {
        device_name: String,
        width: u32,
        height: u32,
    },
    /// Pen / stylus / eraser only. Single tool with pressure + proximity.
    /// Finger touches go through `Touches` for multi-touch support.
    Pointer {
        x: f32,
        y: f32,
        pressed: bool,
        pressure: f32,
        tool: PointerTool,
        in_range: bool,
    },
    /// Snapshot of currently-down fingers. Empty list = all released. The host
    /// diffs against the previous snapshot to emit Linux MT-B events.
    Touches {
        points: Vec<TouchPoint>,
    },
    Pong,
}
