mod capture;
mod h264_dump;
mod h264_stream;

use anyhow::{Context, Result};
use capture::{FrameRx, start as start_capture};
use ferrite_core::{HostMessage, PixelFormat};
use h264_stream::H264Encoder;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{error, info, warn};

const ADDR: &str = "0.0.0.0:7543";
const STREAM_FPS: u32 = 60;
const READ_CHUNK: usize = 64 * 1024;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let (tx, rx) = watch::channel(None);

    if let Err(e) = start_capture(tx).await {
        warn!(error = %e, "screen capture failed to start; host has nothing to stream");
    }

    let listener = TcpListener::bind(ADDR)
        .await
        .with_context(|| format!("bind {ADDR}"))?;
    info!("ferrite host listening on {ADDR}");
    loop {
        let (sock, peer) = listener.accept().await?;
        let rx = rx.clone();
        info!(%peer, "client connected");
        tokio::spawn(async move {
            if let Err(e) = handle(sock, rx).await {
                error!(%peer, error = %e, "client handler failed");
            } else {
                info!(%peer, "client handler done");
            }
        });
    }
}

async fn handle(sock: TcpStream, mut rgb_rx: FrameRx) -> Result<()> {
    // Wait for the first RGB frame so we know dimensions before spawning ffmpeg.
    let (width, height) = loop {
        if let Some(f) = rgb_rx.borrow().as_ref() {
            break (f.width, f.height);
        }
        rgb_rx.changed().await.context("capture source ended")?;
    };

    let mut enc = H264Encoder::spawn(width, height, STREAM_FPS)
        .with_context(|| format!("spawn h264 encoder for {}x{}", width, height))?;
    let stdin = enc.take_stdin().context("no stdin")?;
    let stdout = enc.take_stdout().context("no stdout")?;

    info!(width, height, fps = STREAM_FPS, "h264 encoder spawned");

    // Drive both halves concurrently. When one errors or finishes, the whole
    // handler returns, dropping `enc` which SIGKILLs ffmpeg, which EOFs the
    // other half immediately.
    let rgb_fut = pump_rgb(rgb_rx, stdin);
    let tcp_fut = pump_h264(stdout, sock, width, height);
    tokio::select! {
        r = rgb_fut => r.context("rgb -> ffmpeg")?,
        r = tcp_fut => r.context("ffmpeg -> tcp")?,
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
    mut sock: TcpStream,
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
        write_frame(&mut sock, &bytes).await?;
    }
}

async fn write_frame(sock: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    sock.write_u32(bytes.len() as u32).await?;
    sock.write_all(bytes).await?;
    Ok(())
}
