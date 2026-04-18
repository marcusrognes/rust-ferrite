//! Minimal AOA protocol test. Runs the FULL wire protocol (SYNC_PREAMBLE →
//! read Hello → send length-prefixed bincode HostMessage::VideoFrame frames →
//! read ClientMessage input events) over a direct rusb bulk pair — no tokio,
//! no bridge, no tasks.
//!
//! Pairs with `AoaProtoActivity` which calls `FerriteLib.streamFd` — the same
//! JNI function MainActivity uses. If this reliably runs for N sessions back
//! to back, the protocol + JNI are clean and the main-app bug is in
//! MainActivity or the tokio bridge.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use ferrite_core::{ClientMessage, HostMessage, PixelFormat};
use rusb::{DeviceHandle, Direction, GlobalContext, TransferType};

const AOA_VID: u16 = 0x18D1;
const AOA_PIDS: [u16; 6] = [0x2D00, 0x2D01, 0x2D02, 0x2D03, 0x2D04, 0x2D05];

// Must match AoaProtoActivity's accessory filter XML.
const MANUFACTURER: &str = "co.dealdrive";
const MODEL: &str = "FerriteProto";
const DESCRIPTION: &str = "Ferrite AOA protocol test";
const VERSION: &str = "1";
const URI: &str = "https://example.invalid/ferrite-aoa-proto";
const SERIAL: &str = "proto-0001";

const REQ_GET_PROTOCOL: u8 = 51;
const REQ_SEND_STRING: u8 = 52;
const REQ_START: u8 = 53;

const USB_TIMEOUT: Duration = Duration::from_secs(2);

/// Matches host::SYNC_MAGIC + padding.
const SYNC_MAGIC: &[u8] = b"FERRITE\0";
const SYNC_PREAMBLE: &[u8] = &{
    let mut a = [0xA5u8; 511];
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

fn main() -> Result<()> {
    let iterations: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    println!("=== AOA full-protocol test ({iterations} iteration(s)) ===");

    for iter in 1..=iterations {
        println!("\n--- iteration {iter}/{iterations} ---");
        ensure_accessory(iter == 1)?;
        run_once(iter == 1)?;
        if iter < iterations {
            thread::sleep(Duration::from_secs(1));
        }
    }
    println!("\n=== all iterations passed ===");
    Ok(())
}

/// Wait for the device to appear in AOA mode. If it's not there, try to
/// switch a non-AOA device into accessory mode, then poll up to `poll_ms`.
/// Between iterations we may land here while Android is still re-enumerating
/// after a libusb_reset_device, so poll generously.
fn ensure_accessory(first: bool) -> Result<()> {
    let poll_secs = if first { 5 } else { 15 };
    for attempt in 0..3 {
        if find_accessory().is_some() {
            return Ok(());
        }
        if attempt > 0 {
            println!("    retry switch_to_accessory (attempt {})", attempt + 1);
        } else {
            println!("[1] searching for AOA-capable device...");
        }
        if switch_to_accessory().is_err() {
            // No candidate yet — device may still be mid-reenumeration.
            thread::sleep(Duration::from_secs(1));
            continue;
        }
        for _ in 0..(poll_secs * 10) {
            if find_accessory().is_some() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    bail!("accessory did not appear after retries")
}

fn run_once(allow_prompt_wait: bool) -> Result<()> {
    let dev = find_accessory().ok_or_else(|| anyhow!("no accessory"))?;
    let mut handle = dev.open().context("open accessory")?;
    handle.set_auto_detach_kernel_driver(true).ok();

    let (iface, ep_in, ep_out) = find_bulk_interface(&dev)?;
    println!("[2] ep_in={ep_in:#04x} ep_out={ep_out:#04x}");
    handle.claim_interface(iface).context("claim_interface")?;
    let handle = Arc::new(handle);

    // Drain stale bytes.
    let mut drained = 0;
    let mut tmp = vec![0u8; 64 * 1024];
    loop {
        match handle.read_bulk(ep_in, &mut tmp, Duration::from_millis(100)) {
            Ok(n) if n > 0 => drained += n,
            _ => break,
        }
    }
    println!("[3] drained {drained} stale bytes");

    // Write SYNC_PREAMBLE. Give Android app time to accept the permission
    // prompt on the first iteration.
    let prompt_timeout = if allow_prompt_wait {
        Duration::from_secs(3)
    } else {
        Duration::from_secs(1)
    };
    let mut attempt = 0;
    let max_attempts = if allow_prompt_wait { 10 } else { 3 };
    loop {
        attempt += 1;
        match handle.write_bulk(ep_out, SYNC_PREAMBLE, prompt_timeout) {
            Ok(n) if n == SYNC_PREAMBLE.len() => break,
            Ok(n) => bail!("short preamble write {n}/{}", SYNC_PREAMBLE.len()),
            Err(rusb::Error::Timeout) if attempt < max_attempts => {
                println!("    preamble attempt {attempt}: timeout, retrying...");
                continue;
            }
            Err(e) => bail!("preamble write: {e}"),
        }
    }
    println!("[4] wrote {}-byte preamble on attempt {attempt}", SYNC_PREAMBLE.len());

    // Read Hello — very generous timeout. On iteration 2+, Android needs
    // ~15-20s to notice the previous session ended (via EIO on its fd) and
    // fire a fresh ACCESSORY_ATTACHED intent that reopens our activity.
    println!("[5] waiting for Hello (may take 20-40s between iterations)...");
    let hello_bytes = read_frame_with_timeout(&handle, ep_in, Duration::from_secs(60))?;
    let hello: ClientMessage = bincode::deserialize(&hello_bytes).context("decode Hello")?;
    match &hello {
        ClientMessage::Hello {
            device_name,
            width,
            height,
        } => println!(
            "[5] got Hello: device={device_name:?} {width}x{height}"
        ),
        other => bail!("expected Hello, got {other:?}"),
    }

    // Spawn a reader to collect input events in parallel with our writes.
    let handle_for_reader = handle.clone();
    let reader = thread::spawn(move || -> Result<Vec<ClientMessage>> {
        let mut events = Vec::new();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            match try_read_frame(&handle_for_reader, ep_in, Duration::from_millis(500))? {
                Some(buf) => {
                    let msg: ClientMessage =
                        bincode::deserialize(&buf).context("decode ClientMessage")?;
                    events.push(msg);
                }
                None => continue,
            }
        }
        Ok(events)
    });

    // Send 50 fake VideoFrame frames at ~60 fps, each ~1 KB payload.
    let frame_count = 50;
    let payload = vec![0xAA_u8; 1024];
    let start = Instant::now();
    for i in 0..frame_count {
        let msg = HostMessage::VideoFrame {
            format: PixelFormat::H264,
            width: 1920,
            height: 1080,
            data: payload.clone(),
        };
        let bytes = bincode::serialize(&msg)?;
        write_frame(&handle, ep_out, &bytes)?;
        if i % 10 == 0 {
            println!("    sent frame {i}/{frame_count}");
        }
        thread::sleep(Duration::from_millis(16));
    }
    let elapsed = start.elapsed();
    println!(
        "[6] sent {frame_count} frames in {:.2?} ({:.1} fps)",
        elapsed,
        frame_count as f32 / elapsed.as_secs_f32()
    );

    let events = reader
        .join()
        .map_err(|_| anyhow!("reader panicked"))?
        .context("reader returned error")?;
    println!("[7] received {} input events", events.len());

    // Force USB re-enumeration so Android sees ACCESSORY_DETACHED →
    // ACCESSORY_ATTACHED and fires a fresh onNewIntent. Without this the
    // Android-side worker stays blocked mid-read and the next iteration's
    // preamble is consumed as a bogus length prefix.
    let handle = Arc::try_unwrap(handle)
        .map_err(|_| anyhow!("reader still holds handle"))?;
    match handle.reset() {
        Ok(()) => println!("[8] libusb_reset_device OK"),
        Err(e) => println!("[8] reset failed (continuing): {e}"),
    }
    drop(handle);
    thread::sleep(Duration::from_millis(500));
    println!("[9] iteration OK");
    Ok(())
}

fn read_frame(handle: &DeviceHandle<GlobalContext>, ep_in: u8) -> Result<Vec<u8>> {
    read_frame_with_timeout(handle, ep_in, Duration::from_secs(5))
}

fn read_frame_with_timeout(
    handle: &DeviceHandle<GlobalContext>,
    ep_in: u8,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut len = [0u8; 4];
    read_exact(handle, ep_in, &mut len, timeout)?;
    let len = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; len];
    read_exact(handle, ep_in, &mut buf, Duration::from_secs(5))?;
    Ok(buf)
}

/// Non-blocking-ish variant: returns Ok(None) on timeout rather than Err.
fn try_read_frame(
    handle: &DeviceHandle<GlobalContext>,
    ep_in: u8,
    timeout: Duration,
) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match handle.read_bulk(ep_in, &mut len, timeout) {
        Ok(4) => {}
        Ok(n) if n == 0 => return Ok(None),
        Ok(n) => bail!("short length read {n}"),
        Err(rusb::Error::Timeout) => return Ok(None),
        Err(e) => bail!("read_bulk: {e}"),
    }
    let len = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; len];
    read_exact(handle, ep_in, &mut buf, Duration::from_secs(5))?;
    Ok(Some(buf))
}

fn read_exact(
    handle: &DeviceHandle<GlobalContext>,
    ep_in: u8,
    buf: &mut [u8],
    timeout: Duration,
) -> Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = handle
            .read_bulk(ep_in, &mut buf[filled..], timeout)
            .with_context(|| format!("read_bulk at {filled}/{}", buf.len()))?;
        if n == 0 {
            bail!("read_exact short at {filled}/{}", buf.len());
        }
        filled += n;
    }
    Ok(())
}

fn write_frame(handle: &DeviceHandle<GlobalContext>, ep_out: u8, bytes: &[u8]) -> Result<()> {
    let len = (bytes.len() as u32).to_be_bytes();
    write_all(handle, ep_out, &len)?;
    write_all(handle, ep_out, bytes)?;
    Ok(())
}

fn write_all(handle: &DeviceHandle<GlobalContext>, ep_out: u8, bytes: &[u8]) -> Result<()> {
    let mut sent = 0;
    while sent < bytes.len() {
        let n = handle
            .write_bulk(ep_out, &bytes[sent..], Duration::from_secs(5))
            .with_context(|| format!("write_bulk at {sent}/{}", bytes.len()))?;
        sent += n;
    }
    Ok(())
}

// --- AOA handshake ---------------------------------------------------------

fn find_accessory() -> Option<rusb::Device<GlobalContext>> {
    rusb::devices().ok()?.iter().find(|d| {
        d.device_descriptor()
            .ok()
            .map(|desc| desc.vendor_id() == AOA_VID && AOA_PIDS.contains(&desc.product_id()))
            .unwrap_or(false)
    })
}

fn switch_to_accessory() -> Result<()> {
    for dev in rusb::devices()?.iter() {
        let desc = match dev.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        if desc.class_code() == 9 {
            continue;
        }
        if desc.vendor_id() == AOA_VID && AOA_PIDS.contains(&desc.product_id()) {
            continue;
        }
        let mut handle = match dev.open() {
            Ok(h) => h,
            Err(_) => continue,
        };
        let mut ver_buf = [0u8; 2];
        let ver = handle.read_control(
            rusb::request_type(
                rusb::Direction::In,
                rusb::RequestType::Vendor,
                rusb::Recipient::Device,
            ),
            REQ_GET_PROTOCOL,
            0,
            0,
            &mut ver_buf,
            USB_TIMEOUT,
        );
        if !matches!(ver, Ok(n) if n >= 2) {
            continue;
        }
        let ver = u16::from_le_bytes(ver_buf);
        if ver < 1 {
            continue;
        }
        println!("    candidate vid={:04x} pid={:04x} ver={ver}", desc.vendor_id(), desc.product_id());
        send_strings(&mut handle)?;
        handle.write_control(
            rusb::request_type(
                rusb::Direction::Out,
                rusb::RequestType::Vendor,
                rusb::Recipient::Device,
            ),
            REQ_START,
            0,
            0,
            &[],
            USB_TIMEOUT,
        )?;
        return Ok(());
    }
    bail!("no AOA-capable device found")
}

fn send_strings(handle: &mut DeviceHandle<GlobalContext>) -> Result<()> {
    for (idx, s) in [
        (0u16, MANUFACTURER),
        (1, MODEL),
        (2, DESCRIPTION),
        (3, VERSION),
        (4, URI),
        (5, SERIAL),
    ] {
        let mut payload: Vec<u8> = s.as_bytes().to_vec();
        payload.push(0);
        handle.write_control(
            rusb::request_type(
                rusb::Direction::Out,
                rusb::RequestType::Vendor,
                rusb::Recipient::Device,
            ),
            REQ_SEND_STRING,
            0,
            idx,
            &payload,
            USB_TIMEOUT,
        )?;
    }
    Ok(())
}

fn find_bulk_interface(dev: &rusb::Device<GlobalContext>) -> Result<(u8, u8, u8)> {
    let cfg = dev.active_config_descriptor()?;
    for iface in cfg.interfaces() {
        for ifdesc in iface.descriptors() {
            let mut ep_in = None;
            let mut ep_out = None;
            for ep in ifdesc.endpoint_descriptors() {
                if ep.transfer_type() != TransferType::Bulk {
                    continue;
                }
                match ep.direction() {
                    Direction::In => ep_in = Some(ep.address()),
                    Direction::Out => ep_out = Some(ep.address()),
                }
            }
            if let (Some(i), Some(o)) = (ep_in, ep_out) {
                return Ok((ifdesc.interface_number(), i, o));
            }
        }
    }
    bail!("no bulk interface")
}
