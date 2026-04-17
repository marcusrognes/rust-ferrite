use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum HostMessage {
    VideoFrame { data: Vec<u8>, width: u32, height: u32 },
    Ping,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    TouchEvent { x: f32, y: f32, pressed: bool },
    Pong,
}
