mod aoa;
mod capture;
mod h264_dump;
mod h264_stream;
mod input;
mod virtual_display;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use capture::{FrameRx, start as start_capture};
use ferrite_core::{ClientMessage, ClientStatus, HostMessage, PixelFormat, Status, status_path};
use h264_stream::H264Encoder;
use input::{InputSink, PEN_NAME_PREFIX, TOUCH_NAME_PREFIX};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{Mutex, watch};
use tracing::{debug, error, info, warn};
use virtual_display::VirtualDisplayHandle;

type Clients = Arc<Mutex<HashMap<String, ClientStatus>>>;

const ADDR: &str = "0.0.0.0:7543";
const STREAM_FPS: u32 = 60;
const READ_CHUNK: usize = 64 * 1024;
/// Preamble the host writes at session start. The client drains bytes until
/// it matches the 8-byte magic prefix, flushing any stale transport bytes.
/// Padding is filler after the magic — enough that the USB bulk write is
/// comfortably above any small-packet coalescing thresholds on the Android
/// side, while still ending on a non-max-packet boundary so a short-packet
/// terminates the transfer.
const SYNC_MAGIC: &[u8] = b"FERRITE\0";
const SYNC_PREAMBLE: &[u8] = &{
    let mut a = [0xA5u8; 511]; // 511 bytes: short-packet-friendly (< 512)
    a[0] = b'F';
    a[1] = b'E';
    a[2] = b'R';
    a[3] = b'R';
    a[4] = b'I';
    a[5] = b'T';
    a[6] = b'E';
    a[7] = 0;
    a
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Mirror,
    Virtual,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mode = match std::env::var("FERRITE_MODE").as_deref() {
        Ok("virtual") => Mode::Virtual,
        _ => Mode::Mirror,
    };

    // Mirror mode runs a single global capture and shares it with all clients.
    // Virtual mode creates a per-client evdi monitor lazily (after Hello), so
    // there's nothing to start up-front.
    let shared_rx = if mode == Mode::Mirror {
        let (tx, rx) = watch::channel(None);
        if let Err(e) = start_capture(tx).await {
            warn!(error = %e, "screen capture failed to start; host has nothing to stream");
        } else {
            info!("FERRITE_MODE=mirror: portal/pipewire capture started");
        }
        Some(rx)
    } else {
        info!("FERRITE_MODE=virtual: waiting for client Hello to create monitor");
        None
    };

    let clients: Clients = Arc::new(Mutex::new(HashMap::new()));

    // Periodic JSON status dump.
    {
        let clients = clients.clone();
        let mode_str = match mode {
            Mode::Virtual => "virtual",
            Mode::Mirror => "mirror",
        }
        .to_string();
        tokio::spawn(async move {
            let path = status_path();
            loop {
                let snap = {
                    let g = clients.lock().await;
                    Status {
                        listen_addr: ADDR.to_string(),
                        mode: mode_str.clone(),
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

    // AOA listener: blocks on a plugged-in Android device entering accessory
    // mode, bridges its bulk endpoints into a tokio AsyncRead+AsyncWrite pair,
    // hands that pair to the same `handle()` the TCP listener uses. Opt-in
    // via FERRITE_AOA=1 — see STATUS.md for the reliability caveat.
    if std::env::var("FERRITE_AOA").ok().as_deref() == Some("1") {
        let shared_rx = shared_rx.clone();
        let clients = clients.clone();
        tokio::spawn(async move {
            loop {
                let stream = match tokio::task::spawn_blocking(aoa::AoaStream::wait_for_device)
                    .await
                {
                    Ok(Ok(s)) => s,
                    Ok(Err(e)) => {
                        warn!(error = %e, "AOA wait_for_device failed");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    Err(e) => {
                        warn!(error = %e, "AOA blocking task panicked");
                        tokio::time::sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };
                aoa::drain_stale(&stream);
                let bridge = aoa::spawn_bridge(std::sync::Arc::new(stream));
                let peer = "aoa".to_string();
                info!("AOA client attached");
                let res = handle(bridge, mode, shared_rx.clone(), &peer, clients.clone()).await;
                clients.lock().await.remove(&peer);
                match res {
                    Ok(()) => info!("AOA stream ended"),
                    Err(e) => error!(error = %e, "AOA handler failed"),
                }
            }
        });
    }

    let listener = TcpListener::bind(ADDR)
        .await
        .with_context(|| format!("bind {ADDR}"))?;
    info!("ferrite host listening on {ADDR}");
    loop {
        let (sock, peer) = listener.accept().await?;
        // Disable Nagle — we already coalesce per-AU writes and want every
        // frame on the wire ASAP.
        let _ = sock.set_nodelay(true);
        // Keepalive so we notice yanked-USB / network-drop clients within
        // seconds, not hours.
        let sock2 = socket2::SockRef::from(&sock);
        let _ = sock2.set_keepalive(true);
        let _ = sock2.set_tcp_keepalive(
            &socket2::TcpKeepalive::new()
                .with_time(Duration::from_secs(5))
                .with_interval(Duration::from_secs(2)),
        );
        let shared_rx = shared_rx.clone();
        let clients = clients.clone();
        info!(%peer, "client connected");
        tokio::spawn(async move {
            let peer_str = peer.to_string();
            let res = handle(sock, mode, shared_rx, &peer_str, clients.clone()).await;
            clients.lock().await.remove(&peer_str);
            match res {
                Ok(()) => info!(%peer, "stream ended"),
                Err(e) => error!(%peer, error = %e, "client handler failed"),
            }
        });
    }
}

async fn handle<S>(
    sock: S,
    mode: Mode,
    shared_rx: Option<FrameRx>,
    peer: &str,
    clients: Clients,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut reader, mut writer) = tokio::io::split(sock);

    // Sync preamble: client scans for the first 8 bytes (SYNC_MAGIC) to
    // flush any stale transport bytes; the rest is filler. Matches
    // `android_jni::SYNC_MAGIC`.
    writer.write_all(SYNC_PREAMBLE).await?;

    // First message must be Hello — drives monitor sizing + device naming.
    let hello = read_hello(&mut reader).await?;
    info!(
        %peer,
        device = %hello.0,
        w = hello.1,
        h = hello.2,
        "client hello"
    );

    // Per-client virtual monitor (handle dropped on scope exit = "unplugged").
    let (mut rgb_rx, _vd_handle): (FrameRx, Option<VirtualDisplayHandle>) = match mode {
        Mode::Virtual => {
            let (tx, rx) = watch::channel(None);
            let h = virtual_display::start(hello.1, hello.2, &hello.0, tx)
                .context("start virtual display")?;
            (rx, Some(h))
        }
        Mode::Mirror => {
            let rx = shared_rx.context("mirror mode but no capture source")?;
            (rx, None)
        }
    };

    // Per-client virtual input devices, named after the client.
    let input_sink = match InputSink::new(&hello.0) {
        Ok(s) => {
            // For virtual mode, find the freshly-created evdi connector and
            // pin our devices to it. For mirror mode there's no canonical
            // target — leave map_to_output unset so input goes to whichever
            // output the cursor's on.
            let map_to = if mode == Mode::Virtual {
                find_evdi_connector().await
            } else {
                None
            };
            if let Err(e) = ensure_cosmic_input_entries(&hello.0, map_to.as_deref()) {
                warn!(error = %e, "couldn't write cosmic input config; mapping may not apply");
            }
            Some(s)
        }
        Err(e) => {
            warn!(error = %e, "virtual pointer disabled; touch events will be dropped");
            None
        }
    };

    // Wait for the first frame so the encoder gets the right dimensions.
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

    let rgb_fut = pump_rgb(rgb_rx, stdin);
    let tcp_fut = pump_h264(stdout, writer, width, height);
    let input_fut = pump_input(reader, input_sink);
    tokio::select! {
        r = rgb_fut => r.context("rgb -> ffmpeg")?,
        r = tcp_fut => r.context("ffmpeg -> tcp")?,
        r = input_fut => r.context("tcp -> input")?,
    }
    drop(enc);
    Ok(())
}

async fn read_hello<R: AsyncRead + Unpin>(reader: &mut R) -> Result<(String, u32, u32)> {
    let len = reader.read_u32().await.context("read hello length")? as usize;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await.context("read hello body")?;
    let msg: ClientMessage = bincode::deserialize(&buf).context("decode hello")?;
    match msg {
        ClientMessage::Hello {
            device_name,
            width,
            height,
        } => Ok((device_name, width, height)),
        _ => bail!("expected Hello as first message"),
    }
}

async fn pump_rgb(mut rgb_rx: FrameRx, mut stdin: tokio::process::ChildStdin) -> Result<()> {
    // Skip feeding ffmpeg when the captured pixels are unchanged from the
    // previous frame — saves the encoder + network the trip on idle screens.
    // xxh3 hashes ~15GB/s so a 2800x1720 RGB frame costs ~1ms.
    let mut last_hash: Option<u64> = None;
    loop {
        let frame = rgb_rx.borrow_and_update().clone();
        if let Some(frame) = frame {
            let h = xxhash_rust::xxh3::xxh3_64(&frame.rgb);
            if Some(h) != last_hash {
                stdin.write_all(&frame.rgb).await?;
                last_hash = Some(h);
            }
        }
        rgb_rx.changed().await.context("capture source ended")?;
    }
}

async fn pump_h264<W: AsyncWrite + Unpin>(
    mut stdout: tokio::process::ChildStdout,
    mut sock: W,
    width: u32,
    height: u32,
) -> Result<()> {
    // One VideoFrame per access unit (delimited by AUD NALs in the stream).
    // Emitting whole AUs means MediaCodec receives complete frames per
    // queueInputBuffer call — partial-NAL chunks were the artifact source.
    let mut acc: Vec<u8> = Vec::with_capacity(READ_CHUNK * 4);
    let mut tmp = vec![0u8; READ_CHUNK];
    let mut au_start: Option<usize> = None;

    loop {
        let n = stdout.read(&mut tmp).await?;
        if n == 0 {
            // EOF — flush whatever's left as a final AU.
            if let Some(start) = au_start.take() {
                if acc.len() > start {
                    send_au(&mut sock, width, height, &acc[start..]).await?;
                }
            }
            return Ok(());
        }
        acc.extend_from_slice(&tmp[..n]);

        // Search for AUD NALs from where we left off; whenever we find one and
        // already have a current AU in progress, ship the completed AU.
        let search_from = au_start.map(|s| s + 1).unwrap_or(0);
        let mut cursor = search_from;
        while let Some(rel) = find_aud(&acc[cursor..]) {
            let aud_pos = cursor + rel;
            if let Some(start) = au_start {
                send_au(&mut sock, width, height, &acc[start..aud_pos]).await?;
            }
            au_start = Some(aud_pos);
            cursor = aud_pos + 4; // skip past start code so we find the *next* AUD
        }

        // Compact: drop bytes before the current AU start so acc doesn't grow forever.
        if let Some(start) = au_start {
            if start > 0 {
                acc.drain(..start);
                au_start = Some(0);
            }
        } else {
            // No AUD seen yet — drop everything to avoid unbounded growth on a
            // misbehaving stream (shouldn't happen with `-aud 1`).
            acc.clear();
        }
    }
}

async fn send_au<W: AsyncWrite + Unpin>(
    sock: &mut W,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<()> {
    let msg = HostMessage::VideoFrame {
        format: PixelFormat::H264,
        width,
        height,
        data: data.to_vec(),
    };
    let bytes = bincode::serialize(&msg)?;
    sock.write_u32(bytes.len() as u32).await?;
    sock.write_all(&bytes).await?;
    Ok(())
}

/// Find the first Annex-B AUD NAL (`00 00 00 01 09` or `00 00 01 09`) in `buf`.
/// Returns the start-code's first byte index, or None.
fn find_aud(buf: &[u8]) -> Option<usize> {
    let mut i = 0;
    while i + 5 <= buf.len() {
        if buf[i] == 0 && buf[i + 1] == 0 {
            if buf[i + 2] == 0 && buf[i + 3] == 1 {
                if (buf[i + 4] & 0x1f) == 9 {
                    return Some(i);
                }
                i += 4;
                continue;
            } else if buf[i + 2] == 1 && (buf[i + 3] & 0x1f) == 9 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

#[derive(Serialize, Deserialize, Debug)]
struct CosmicInputEntry {
    state: CosmicInputState,
    #[serde(skip_serializing_if = "Option::is_none")]
    map_to_output: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
enum CosmicInputState {
    Enabled,
    #[allow(dead_code)]
    Disabled,
    #[allow(dead_code)]
    DisabledOnExternalMouse,
}

/// Ensure cosmic's `input_devices` config has entries for our two virtual
/// devices (touchscreen + pen) suffixed with the client name, optionally
/// pinned to `map_to`. cosmic-comp reads this file via inotify, so writing
/// it both adds the mapping and re-evaluates against the live device list.
///
/// Other entries in the file are preserved across the round-trip when their
/// fields fit `CosmicInputEntry` — unknown fields will be dropped, so this is
/// best-effort.
fn ensure_cosmic_input_entries(device_name: &str, map_to: Option<&str>) -> Result<()> {
    let Some(home) = std::env::var_os("HOME") else {
        bail!("$HOME not set");
    };
    let path = std::path::PathBuf::from(home)
        .join(".config/cosmic/com.system76.CosmicComp/v1/input_devices");
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let mut map: BTreeMap<String, CosmicInputEntry> = match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => ron::from_str(&s).unwrap_or_default(),
        _ => BTreeMap::new(),
    };

    for prefix in [TOUCH_NAME_PREFIX, PEN_NAME_PREFIX] {
        let name = format!("{prefix} ({device_name})");
        map.insert(
            name,
            CosmicInputEntry {
                state: CosmicInputState::Enabled,
                map_to_output: map_to.map(str::to_string),
            },
        );
    }

    let serialized = ron::ser::to_string_pretty(&map, ron::ser::PrettyConfig::default())?;
    std::fs::write(&path, serialized)?;
    info!(?path, device = device_name, ?map_to, "wrote cosmic input config");
    Ok(())
}

/// Scan `/sys/class/drm` for a `cardN-CONNECTOR` entry whose underlying device
/// is an evdi card and that's currently `connected`. Returns the connector
/// name (e.g. "DVI-I-1") to use as `map_to_output`. Retries briefly because
/// evdi_connect → kernel-side hotplug → sysfs `connected` takes a moment.
async fn find_evdi_connector() -> Option<String> {
    for _ in 0..20 {
        if let Some(c) = scan_evdi_connector() {
            return Some(c);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

fn scan_evdi_connector() -> Option<String> {
    let dir = std::fs::read_dir("/sys/class/drm").ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("card") {
            continue;
        }
        let Some((card, conn)) = name.split_once('-') else {
            continue;
        };
        let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        if status.trim() != "connected" {
            continue;
        }
        // The connector's `device` link points back at the card; check the
        // card's `device/uevent` to learn its driver. evdi cards report
        // `DRIVER=evdi`; AMD/Intel report `DRIVER=amdgpu`/`i915`.
        let uevent = std::fs::read_to_string(format!("/sys/class/drm/{card}/device/uevent"))
            .unwrap_or_default();
        if uevent.lines().any(|l| l == "DRIVER=evdi") {
            return Some(conn.to_string());
        }
    }
    None
}

async fn pump_input<R: AsyncRead + Unpin>(
    mut reader: R,
    input: Option<InputSink>,
) -> Result<()> {
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
                in_range,
            } => {
                debug!(?tool, x, y, pressed, pressure, in_range, "pointer");
                if let Some(s) = input.as_ref() {
                    s.send_pointer(x, y, pressed, pressure, tool, in_range);
                }
            }
            ClientMessage::Touches { points } => {
                debug!(n = points.len(), "touches");
                if let Some(s) = input.as_ref() {
                    s.send_touches(&points);
                }
            }
            ClientMessage::Hello { .. } => {} // ignore stray re-hellos
            ClientMessage::Pong => {}
        }
    }
}
