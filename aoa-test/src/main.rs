//! Minimal AOA echo test.
//!
//! 1. Find a USB device that speaks AOA, send it the accessory strings + START
//!    control transfer so it re-enumerates as an accessory (VID 18D1, PID 2D0x).
//! 2. Claim the accessory interface's bulk pipes.
//! 3. Write a 256-byte pattern. Read the same number of bytes back. Verify.
//!
//! Start the matching `AoaEchoActivity` on the Android side before running
//! this. Prints diagnostic info + pass/fail.

use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rusb::{DeviceHandle, Direction, GlobalContext, TransferType};

const AOA_VID: u16 = 0x18D1;
const AOA_PIDS: [u16; 6] = [0x2D00, 0x2D01, 0x2D02, 0x2D03, 0x2D04, 0x2D05];

// Match the strings against the AoaEchoActivity intent filter exactly.
const MANUFACTURER: &str = "co.dealdrive";
const MODEL: &str = "FerriteEcho";
const DESCRIPTION: &str = "Ferrite AOA echo test";
const VERSION: &str = "1";
const URI: &str = "https://example.invalid/ferrite-aoa-test";
const SERIAL: &str = "echo-0001";

const REQ_GET_PROTOCOL: u8 = 51;
const REQ_SEND_STRING: u8 = 52;
const REQ_START: u8 = 53;

const USB_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> Result<()> {
    // args: [iterations] [payload_bytes]
    let mut args = std::env::args().skip(1);
    let iterations: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1);
    let payload_size: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(256);
    println!(
        "=== Ferrite AOA echo test ({iterations} iteration(s), {payload_size} bytes each) ==="
    );

    // Step 1: if already in accessory mode, skip the switch.
    if find_accessory().is_some() {
        println!("[1] device already in accessory mode");
    } else {
        println!("[1] searching for AOA-capable device...");
        switch_to_accessory().context("switch to accessory mode")?;
        println!("[1] START sent; waiting for re-enumeration");
        for i in 0..50 {
            if find_accessory().is_some() {
                println!("[1] accessory appeared after {}00ms", i);
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    for iter in 1..=iterations {
        println!("\n--- iteration {iter}/{iterations} ---");
        run_once(iter == 1, payload_size)?;
        if iter < iterations {
            println!("... sleeping 1s before next iteration");
            thread::sleep(Duration::from_secs(1));
        }
    }
    println!("\n=== all {iterations} iterations passed ===");
    Ok(())
}

fn run_once(allow_prompt_wait: bool, payload_size: usize) -> Result<()> {
    use std::sync::Arc;
    let dev = find_accessory().ok_or_else(|| anyhow!("no accessory"))?;
    let mut handle = dev.open().context("open accessory")?;
    handle.set_auto_detach_kernel_driver(true).ok();

    let (iface, ep_in, ep_out) = find_bulk_interface(&dev)?;
    println!("[2] interface: iface={iface} ep_in={ep_in:#04x} ep_out={ep_out:#04x}");
    handle.claim_interface(iface).context("claim_interface")?;

    // Drain stale bytes from prior runs.
    let mut drained = 0;
    let mut tmp = vec![0u8; 64 * 1024];
    loop {
        match handle.read_bulk(ep_in, &mut tmp, Duration::from_millis(100)) {
            Ok(n) if n > 0 => drained += n,
            _ => break,
        }
    }
    println!("[3] drained {drained} stale bytes");

    // Pattern uses 0..0xFE so heartbeat markers (0xFF 0xFF) are separable.
    let pattern: Vec<u8> = (0..payload_size).map(|i| (i % 0xff) as u8).collect();
    println!(
        "[4] writing {} bytes + concurrently reading echo (0xFFFF = heartbeat)",
        pattern.len()
    );

    // rusb::DeviceHandle is Sync; safe to read and write from parallel threads.
    let handle = Arc::new(handle);
    let pattern_arc = Arc::new(pattern);
    let reply_size = pattern_arc.len();

    // Reader thread: pulls echo + heartbeat bytes off the IN endpoint
    // concurrently with the main thread's write. Demuxes 0xFF 0xFF markers
    // out of the stream, counts them, and accumulates echoed pattern bytes
    // until reply_size pattern bytes have been received.
    let rh = handle.clone();
    let reader = thread::spawn(move || -> Result<(Vec<u8>, u32)> {
        let mut echoed: Vec<u8> = Vec::with_capacity(reply_size);
        let mut hb_count: u32 = 0;
        let mut buf = vec![0u8; 64 * 1024];
        let mut pending_ff = false; // split heartbeat across read boundaries
        while echoed.len() < reply_size {
            let n = rh
                .read_bulk(ep_in, &mut buf, Duration::from_secs(10))
                .with_context(|| format!("read_bulk at {} echoed", echoed.len()))?;
            let mut i = 0;
            while i < n {
                let b = buf[i];
                if pending_ff {
                    if b == 0xff {
                        hb_count += 1;
                    } else {
                        echoed.push(0xff);
                        echoed.push(b);
                    }
                    pending_ff = false;
                    i += 1;
                } else if b == 0xff {
                    pending_ff = true;
                    i += 1;
                } else {
                    echoed.push(b);
                    i += 1;
                }
            }
        }
        Ok((echoed, hb_count))
    });

    // Writer on the main thread.
    let wh = handle.clone();
    let p = pattern_arc.clone();
    let prompt_timeout = if allow_prompt_wait {
        Duration::from_secs(3)
    } else {
        Duration::from_secs(1)
    };
    let prompt_attempts = if allow_prompt_wait { 10 } else { 3 };
    let mut sent = 0;
    // First write may need retries until Android accepts the accessory prompt.
    {
        let mut attempt = 0;
        let n = loop {
            attempt += 1;
            match wh.write_bulk(ep_out, &p[..], prompt_timeout) {
                Ok(n) => break n,
                Err(rusb::Error::Timeout) if attempt < prompt_attempts => {
                    println!("    write attempt {attempt}: timeout, retrying...");
                    continue;
                }
                Err(e) => bail!("write_bulk: {e}"),
            }
        };
        sent = n;
        println!("[4] first write {n} bytes (total {sent}/{reply_size})");
    }
    while sent < pattern_arc.len() {
        let n = wh
            .write_bulk(ep_out, &pattern_arc[sent..], Duration::from_secs(10))
            .with_context(|| format!("continuation write at {sent}"))?;
        sent += n;
    }
    println!("[4] written OK");

    let (reply, hb_count) = reader
        .join()
        .map_err(|_| anyhow!("reader thread panicked"))??;
    println!(
        "[5] read {} echoed bytes + {} heartbeats",
        reply.len(),
        hb_count
    );

    if reply == *pattern_arc {
        println!("[6] ECHO MATCHES ({} bytes, {} heartbeats)", reply.len(), hb_count);
    } else {
        println!("[6] MISMATCH:");
        for (i, (s, r)) in pattern_arc.iter().zip(reply.iter()).enumerate() {
            if s != r {
                println!("    byte {i}: sent {s:#04x} got {r:#04x}");
                if i > 10 {
                    println!("    (more, truncated)");
                    break;
                }
            }
        }
        bail!("echo content mismatch");
    }

    drop(handle);
    thread::sleep(Duration::from_millis(100));
    Ok(())
}

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
            continue; // hub
        }
        if desc.vendor_id() == AOA_VID && AOA_PIDS.contains(&desc.product_id()) {
            continue; // already accessory
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
        println!(
            "    candidate: vid={:04x} pid={:04x} aoa_ver={ver}",
            desc.vendor_id(),
            desc.product_id()
        );
        send_strings(&mut handle).context("send accessory strings")?;
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
    bail!("no AOA-capable device found — plug in an Android phone / tablet");
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
    bail!("no bulk interface on accessory device")
}
