//! Android Open Accessory (AOA) protocol transport.
//!
//! AOA lets a Linux host act as a USB host to an Android device (which is
//! normally itself a host for USB peripherals). The Android side re-enumerates
//! as a bulk-pipe "accessory" after a vendor-specific control handshake, then
//! host + device shuffle bytes over two bulk endpoints just like a TCP socket.
//!
//! Benefits over adb-reverse: no developer mode, no adb daemon, no
//! `adb reverse` setup dance. Plug cable in, Android's intent system launches
//! our app, we're on the wire.
//!
//! Flow:
//! 1. Enumerate USB, find any device that *could* speak AOA (ADB-capable
//!    vendor/product ids or anything that answers the protocol probe).
//! 2. Send GetProtocol control request — if the device answers with ≥ 1,
//!    it understands AOA.
//! 3. Send manufacturer / model / description / version / URI / serial
//!    identifier strings so the Android app's intent filter can match us.
//! 4. Send Start — the device briefly disconnects and re-enumerates with
//!    vendor id 0x18D1, product id 0x2D00..=0x2D05 (the accessory PIDs).
//! 5. Open the new device, claim the accessory interface, read/write bulk.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rusb::{DeviceHandle, Direction, GlobalContext, TransferType};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tracing::{debug, info, warn};

pub const MANUFACTURER: &str = "co.dealdrive";
pub const MODEL: &str = "Ferrite";
pub const DESCRIPTION: &str = "Ferrite host";
pub const VERSION: &str = "1";
pub const URI: &str = "https://github.com/marcusrognes/ferrite";
pub const SERIAL: &str = "0000000001";

const AOA_VID: u16 = 0x18D1;
const AOA_PIDS: [u16; 6] = [0x2D00, 0x2D01, 0x2D02, 0x2D03, 0x2D04, 0x2D05];

const REQ_GET_PROTOCOL: u8 = 51;
const REQ_SEND_STRING: u8 = 52;
const REQ_START: u8 = 53;

const STR_MANUFACTURER: u16 = 0;
const STR_MODEL: u16 = 1;
const STR_DESCRIPTION: u16 = 2;
const STR_VERSION: u16 = 3;
const STR_URI: u16 = 4;
const STR_SERIAL: u16 = 5;

const USB_TIMEOUT: Duration = Duration::from_secs(1);

pub struct AoaStream {
    handle: DeviceHandle<GlobalContext>,
    iface: u8,
    ep_in: u8,
    ep_out: u8,
}

impl AoaStream {
    /// Blocks until an AOA-capable Android device is plugged in and enters
    /// accessory mode. Returns the opened bulk stream ready for read/write.
    pub fn wait_for_device() -> Result<Self> {
        loop {
            match try_switch_one() {
                Ok(Some(h)) => return Ok(h),
                Ok(None) => {}
                Err(e) => debug!(error = %e, "aoa switch attempt failed"),
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Returns `Ok(0)` on timeout — callers should just try again. Other
    /// rusb errors (disconnect, pipe) are surfaced as `Err`.
    pub fn read(&self, buf: &mut [u8]) -> Result<usize> {
        match self.handle.read_bulk(self.ep_in, buf, USB_TIMEOUT) {
            Ok(n) => Ok(n),
            Err(rusb::Error::Timeout) => Ok(0),
            Err(e) => Err(anyhow!("bulk read: {e}")),
        }
    }

    pub fn write(&self, buf: &[u8]) -> Result<usize> {
        // Writes shouldn't time out under normal conditions (device consumes
        // the bulk pipe quickly). If they do, surface as error.
        self.handle
            .write_bulk(self.ep_out, buf, USB_TIMEOUT)
            .map_err(|e| anyhow!("bulk write: {e}"))
    }
}

impl Drop for AoaStream {
    fn drop(&mut self) {
        let _ = self.handle.release_interface(self.iface);
    }
}

// rusb::DeviceHandle is Send + Sync; read_bulk / write_bulk take &self and the
// IN / OUT endpoints are independent on the device side, so it's safe to run
// them in parallel from different tokio tasks.
unsafe impl Sync for AoaStream {}

/// Drain any bytes left in the bulk IN endpoint from a previous session.
/// Without this, when a new client connects to an already-accessory-mode
/// device, stale writes from the prior Android app session are still queued
/// and desync our protocol read on the host side.
pub fn drain_stale(stream: &AoaStream) {
    let mut total = 0;
    let mut buf = vec![0u8; 64 * 1024];
    for _ in 0..50 {
        match stream.read(&mut buf) {
            Ok(0) => break, // timeout = queue empty
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    if total > 0 {
        info!(total, "drained stale AOA bytes");
    }
}

/// Bridge an `AoaStream` (blocking rusb I/O) into a tokio `AsyncRead + AsyncWrite`
/// duplex pair. Spawns two tasks that shuffle bytes between the bulk endpoints
/// and the bridge's internal in-memory pipe; returns the other end of that
/// pipe, which looks and behaves like a TCP stream to the rest of the host.
pub fn spawn_bridge(stream: Arc<AoaStream>) -> DuplexStream {
    let (client_side, bridge_side) = tokio::io::duplex(64 * 1024);
    let (mut brx, mut btx) = tokio::io::split(bridge_side);

    // AOA IN  -> bridge TX  (bytes flow from device toward tokio consumer).
    // `AoaStream::read` returns `Ok(0)` on USB timeout which is normal during
    // idle — we just spin and wait for real input. Hard errors (disconnect,
    // pipe stall) break the loop and the bridge closes.
    {
        let s = stream.clone();
        tokio::spawn(async move {
            loop {
                let s = s.clone();
                let res = tokio::task::spawn_blocking(move || {
                    let mut buf = vec![0u8; 64 * 1024];
                    s.read(&mut buf).map(|n| {
                        buf.truncate(n);
                        buf
                    })
                })
                .await;
                match res {
                    Ok(Ok(buf)) if !buf.is_empty() => {
                        if btx.write_all(&buf).await.is_err() {
                            break;
                        }
                    }
                    Ok(Ok(_)) => continue, // timeout, keep polling
                    Ok(Err(e)) => {
                        debug!(error = %e, "aoa read ended");
                        break;
                    }
                    Err(e) => {
                        warn!(error = %e, "aoa read task panicked");
                        break;
                    }
                }
            }
        });
    }

    // bridge RX -> AOA OUT  (bytes flow from tokio producer toward device).
    // Chunk writes — large IDR frames (200-500 KB) as a single bulk write
    // trip up some Android USB stacks and stall the pipe. The chunk size is
    // deliberately NOT a multiple of 512 (USB 2.0 HS bulk max-packet-size)
    // so each chunk ends in a short packet that marks transfer end — which
    // Android's accessory driver needs to release bytes to userspace.
    {
        const CHUNK: usize = 16 * 1024 - 1;
        let s = stream;
        tokio::spawn(async move {
            let mut buf = vec![0u8; CHUNK];
            loop {
                let n = match brx.read(&mut buf).await {
                    Ok(n) if n > 0 => n,
                    _ => break,
                };
                let data = buf[..n].to_vec();
                let head: Vec<String> = data.iter().take(16).map(|b| format!("{b:02x}")).collect();
                info!(n = data.len(), head = head.join(" "), "aoa bridge: forwarding to device");
                let s = s.clone();
                let res = tokio::task::spawn_blocking(move || {
                    let mut offset = 0;
                    while offset < data.len() {
                        let end = (offset + CHUNK).min(data.len());
                        s.write(&data[offset..end])?;
                        offset = end;
                    }
                    Ok::<_, anyhow::Error>(())
                })
                .await;
                match res {
                    Ok(Ok(())) => {}
                    Ok(Err(e)) => {
                        debug!(error = %e, "aoa bulk write failed — bridge closing");
                        break;
                    }
                    Err(e) => {
                        warn!(error = %e, "aoa write task panicked");
                        break;
                    }
                }
            }
        });
    }

    client_side
}

/// One pass through the USB device list: for each device that isn't already
/// in accessory mode, try to switch it. If any device *is* already in
/// accessory mode, open and return it.
fn try_switch_one() -> Result<Option<AoaStream>> {
    // First: is a device already in accessory mode?
    if let Some(h) = open_accessory_if_present()? {
        return Ok(Some(h));
    }

    // Second: try to put a candidate device into accessory mode.
    for dev in rusb::devices()?.iter() {
        let desc = match dev.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        if desc.vendor_id() == AOA_VID && AOA_PIDS.contains(&desc.product_id()) {
            // Already an accessory — handled above.
            continue;
        }
        // Skip hubs + usual non-android stuff to keep logs quiet.
        if desc.class_code() == 9 {
            continue; // hub
        }
        let vid = format!("{:04x}", desc.vendor_id());
        let pid = format!("{:04x}", desc.product_id());
        // Most devices on the bus aren't Android, so access-denied is expected
        // and noisy to log. Keep it at debug.
        let mut handle = match dev.open() {
            Ok(h) => h,
            Err(e) => {
                debug!(vid, pid, error = %e, "open failed");
                continue;
            }
        };
        match query_aoa_version(&handle) {
            Ok(ver) if ver >= 1 => {
                info!(vid, pid, ver, "device speaks AOA, switching to accessory mode");
                if let Err(e) = send_strings_and_start(&mut handle) {
                    warn!(error = %e, "AOA switch failed");
                    continue;
                }
                return Ok(None);
            }
            Ok(ver) => {
                debug!(vid, pid, ver, "AOA version 0 — device does not support AOA");
            }
            Err(e) => {
                debug!(vid, pid, error = %e, "AOA probe failed — not Android");
            }
        }
    }
    Ok(None)
}

fn open_accessory_if_present() -> Result<Option<AoaStream>> {
    for dev in rusb::devices()?.iter() {
        let desc = match dev.device_descriptor() {
            Ok(d) => d,
            Err(_) => continue,
        };
        if desc.vendor_id() != AOA_VID || !AOA_PIDS.contains(&desc.product_id()) {
            continue;
        }
        let handle = dev.open().context("open accessory device")?;
        let cfg = dev.active_config_descriptor().context("active config")?;
        // First interface with two bulk endpoints is the accessory interface.
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
                if let (Some(in_ep), Some(out_ep)) = (ep_in, ep_out) {
                    let num = ifdesc.interface_number();
                    handle.set_auto_detach_kernel_driver(true).ok();
                    handle
                        .claim_interface(num)
                        .with_context(|| format!("claim interface {num}"))?;
                    info!(
                        in_ep = format!("{:#04x}", in_ep),
                        out_ep = format!("{:#04x}", out_ep),
                        "AOA accessory opened"
                    );
                    return Ok(Some(AoaStream {
                        handle,
                        iface: num,
                        ep_in: in_ep,
                        ep_out: out_ep,
                    }));
                }
            }
        }
        bail!("accessory device has no bulk interface");
    }
    Ok(None)
}

fn query_aoa_version(handle: &DeviceHandle<GlobalContext>) -> Result<u16> {
    let mut buf = [0u8; 2];
    let n = handle
        .read_control(
            rusb::request_type(
                rusb::Direction::In,
                rusb::RequestType::Vendor,
                rusb::Recipient::Device,
            ),
            REQ_GET_PROTOCOL,
            0,
            0,
            &mut buf,
            USB_TIMEOUT,
        )
        .map_err(|e| anyhow!("{e}"))?;
    if n < 2 {
        bail!("short response: {n} bytes");
    }
    Ok(u16::from_le_bytes(buf))
}

fn send_strings_and_start(handle: &mut DeviceHandle<GlobalContext>) -> Result<()> {
    for (idx, s) in [
        (STR_MANUFACTURER, MANUFACTURER),
        (STR_MODEL, MODEL),
        (STR_DESCRIPTION, DESCRIPTION),
        (STR_VERSION, VERSION),
        (STR_URI, URI),
        (STR_SERIAL, SERIAL),
    ] {
        let mut payload: Vec<u8> = s.as_bytes().to_vec();
        payload.push(0); // null-terminator, per spec
        handle
            .write_control(
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
            )
            .with_context(|| format!("send string {idx}"))?;
    }
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
    Ok(())
}
