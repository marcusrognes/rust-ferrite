mod capture;

use anyhow::{Context, Result};
use capture::{Frame, FrameRx};
use ferrite_core::HostMessage;
use std::sync::Arc;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tracing::{error, info, warn};

const ADDR: &str = "0.0.0.0:7543";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let (tx, rx) = watch::channel::<Option<Arc<Frame>>>(None);

    if let Err(e) = capture::start(tx).await {
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
                error!(%peer, error = %e, "stream ended");
            } else {
                info!(%peer, "stream ended");
            }
        });
    }
}

async fn handle(mut sock: TcpStream, mut rx: FrameRx) -> Result<()> {
    loop {
        rx.changed().await.context("capture source ended")?;
        let frame = rx.borrow_and_update().clone();
        let Some(frame) = frame else { continue };

        let msg = HostMessage::VideoFrame {
            format: frame.format,
            width: frame.width,
            height: frame.height,
            data: frame.data.clone(),
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
