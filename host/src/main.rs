mod capture;
mod h264_dump;
mod h264_stream;
mod input;
mod virtual_display;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use capture::{FrameRx, start as start_capture};
use ferrite_core::{ClientMessage, ClientStatus, HostMessage, PixelFormat, Status, status_path};
use h264_stream::H264Encoder;
use input::InputSink;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{Mutex, watch};
use tracing::{error, info, warn};

type Clients = Arc<Mutex<HashMap<String, ClientStatus>>>;

const ADDR: &str = "0.0.0.0:7543";
const STREAM_FPS: u32 = 60;
const READ_CHUNK: usize = 64 * 1024;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let (tx, rx) = watch::channel(None);

    let mode = std::env::var("FERRITE_MODE").unwrap_or_else(|_| "mirror".into());
    let mode_for_status = mode.clone();
    match mode.as_str() {
        "virtual" => {
            if let Err(e) = virtual_display::start(tx) {
                warn!(error = %e, "virtual display failed to start; host has nothing to stream");
            } else {
                info!("FERRITE_MODE=virtual: evdi virtual monitor started");
            }
        }
        _ => {
            if let Err(e) = start_capture(tx).await {
                warn!(error = %e, "screen capture failed to start; host has nothing to stream");
            } else {
                info!("FERRITE_MODE=mirror: portal/pipewire capture started");
            }
        }
    }

    let input_sink = match InputSink::new() {
        Ok(s) => Some(s),
        Err(e) => {
            warn!(error = %e, "virtual pointer disabled; touch events will be dropped");
            None
        }
    };

    let clients: Clients = Arc::new(Mutex::new(HashMap::new()));

    // Periodic JSON status dump.
    {
        let clients = clients.clone();
        let mode = mode_for_status;
        tokio::spawn(async move {
            let path = status_path();
            loop {
                let snap = {
                    let g = clients.lock().await;
                    Status {
                        listen_addr: ADDR.to_string(),
                        mode: mode.clone(),
                        clients: g.values().cloned().collect(),
                    }
                };
                if let Ok(j) = serde_json::to_string(&snap) {
                    let _ = tokio::fs::write(&path, j).await;
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }

    let listener = TcpListener::bind(ADDR)
        .await
        .with_context(|| format!("bind {ADDR}"))?;
    info!("ferrite host listening on {ADDR}");
    loop {
        let (sock, peer) = listener.accept().await?;
        let rx = rx.clone();
        let input = input_sink.clone();
        let clients = clients.clone();
        info!(%peer, "client connected");
        tokio::spawn(async move {
            let peer_str = peer.to_string();
            let res = handle(sock, rx, input, &peer_str, clients.clone()).await;
            clients.lock().await.remove(&peer_str);
            match res {
                Ok(()) => info!(%peer, "stream ended"),
                Err(e) => error!(%peer, error = %e, "client handler failed"),
            }
        });
    }
}

async fn handle(
    sock: tokio::net::TcpStream,
    mut rgb_rx: FrameRx,
    input: Option<InputSink>,
    peer: &str,
    clients: Clients,
) -> Result<()> {
    // Wait for the first RGB frame so we know dimensions before spawning ffmpeg.
    let (width, height) = loop {
        if let Some(f) = rgb_rx.borrow().as_ref() {
            break (f.width, f.height);
        }
        rgb_rx.changed().await.context("capture source ended")?;
    };

    clients.lock().await.insert(
        peer.to_string(),
        ClientStatus {
            peer: peer.to_string(),
            width,
            height,
        },
    );

    let mut enc = H264Encoder::spawn(width, height, STREAM_FPS)
        .with_context(|| format!("spawn h264 encoder for {}x{}", width, height))?;
    let stdin = enc.take_stdin().context("no stdin")?;
    let stdout = enc.take_stdout().context("no stdout")?;

    info!(width, height, fps = STREAM_FPS, "h264 encoder spawned");

    let (reader, writer) = sock.into_split();

    let rgb_fut = pump_rgb(rgb_rx, stdin);
    let tcp_fut = pump_h264(stdout, writer, width, height);
    let input_fut = pump_input(reader, input);
    tokio::select! {
        r = rgb_fut => r.context("rgb -> ffmpeg")?,
        r = tcp_fut => r.context("ffmpeg -> tcp")?,
        r = input_fut => r.context("tcp -> input")?,
    }
    drop(enc);
    Ok(())
}

async fn pump_rgb(mut rgb_rx: FrameRx, mut stdin: tokio::process::ChildStdin) -> Result<()> {
    loop {
        let frame = rgb_rx.borrow_and_update().clone();
        if let Some(frame) = frame {
            stdin.write_all(&frame.rgb).await?;
        }
        rgb_rx.changed().await.context("capture source ended")?;
    }
}

async fn pump_h264(
    mut stdout: tokio::process::ChildStdout,
    mut sock: OwnedWriteHalf,
    width: u32,
    height: u32,
) -> Result<()> {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let n = stdout.read(&mut buf).await?;
        if n == 0 {
            return Ok(());
        }
        let msg = HostMessage::VideoFrame {
            format: PixelFormat::H264,
            width,
            height,
            data: buf[..n].to_vec(),
        };
        let bytes = bincode::serialize(&msg)?;
        sock.write_u32(bytes.len() as u32).await?;
        sock.write_all(&bytes).await?;
    }
}

async fn pump_input(mut reader: OwnedReadHalf, input: Option<InputSink>) -> Result<()> {
    loop {
        let len = reader.read_u32().await? as usize;
        let mut buf = vec![0u8; len];
        reader.read_exact(&mut buf).await?;
        let msg: ClientMessage = bincode::deserialize(&buf)?;
        match msg {
            ClientMessage::Pointer {
                x,
                y,
                pressed,
                pressure,
                tool,
            } => {
                if let Some(s) = input.as_ref() {
                    s.send(x, y, pressed, pressure, tool);
                }
            }
            ClientMessage::Pong => {}
        }
    }
}
