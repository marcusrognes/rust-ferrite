use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PixelFormat {
    /// Row-major, 8-bit-per-channel, R,G,B,A byte order per pixel. `data` length = width*height*4.
    Rgba8,
    /// Standard JPEG bytes. `data` is the encoded byte stream.
    Jpeg,
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

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    TouchEvent { x: f32, y: f32, pressed: bool },
    Pong,
}
