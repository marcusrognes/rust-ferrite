use anyhow::{Context, Result, bail};
use ferrite_core::{ClientMessage, HostMessage};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tracing::{error, info};

const ADDR: &str = "0.0.0.0:7543";

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let listener = TcpListener::bind(ADDR)
        .await
        .with_context(|| format!("bind {ADDR}"))?;
    info!("ferrite host listening on {ADDR}");
    loop {
        let (sock, peer) = listener.accept().await?;
        info!(%peer, "client connected");
        tokio::spawn(async move {
            if let Err(e) = handle(sock).await {
                error!(%peer, error = %e, "client handler failed");
            } else {
                info!(%peer, "client handler done");
            }
        });
    }
}

async fn handle(mut sock: TcpStream) -> Result<()> {
    write_frame(&mut sock, &bincode::serialize(&HostMessage::Ping)?).await?;
    let buf = read_frame(&mut sock).await?;
    let msg: ClientMessage = bincode::deserialize(&buf)?;
    match msg {
        ClientMessage::Pong => info!("got Pong"),
        other => bail!("unexpected message: {other:?}"),
    }
    Ok(())
}

async fn write_frame(sock: &mut TcpStream, bytes: &[u8]) -> Result<()> {
    sock.write_u32(bytes.len() as u32).await?;
    sock.write_all(bytes).await?;
    Ok(())
}

async fn read_frame(sock: &mut TcpStream) -> Result<Vec<u8>> {
    let len = sock.read_u32().await? as usize;
    let mut buf = vec![0u8; len];
    sock.read_exact(&mut buf).await?;
    Ok(buf)
}
