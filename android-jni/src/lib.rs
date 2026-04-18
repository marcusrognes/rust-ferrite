//! JNI layer for the Android client. Exposes the Ferrite wire protocol over
//! any bidirectional byte stream — currently TCP (Wi-Fi or adb-reverse) and
//! AOA bulk fds. Both transports converge on a single `run_protocol` helper;
//! adding a new transport means writing one more entry point that hands a
//! `Read + Write` to that helper.

use std::ffi::CString;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use ferrite_core::{ClientMessage, HostMessage, PixelFormat, PointerTool, TouchPoint};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JFloatArray, JIntArray, JObject, JString, JValue};
use jni::sys::{jboolean, jfloat, jint, jstring};

fn android_log(s: &str) {
    extern "C" {
        fn __android_log_write(prio: i32, tag: *const u8, text: *const u8) -> i32;
    }
    let tag = CString::new("ferrite-jni").unwrap();
    let msg = CString::new(s).unwrap();
    unsafe {
        __android_log_write(
            4, // INFO
            tag.as_ptr() as *const u8,
            msg.as_ptr() as *const u8,
        );
    }
}

/// Full preamble the host writes at session start. Must be exactly this many
/// bytes and start with [`SYNC_MAGIC`]; the rest is filler the client
/// discards. Reading the full preamble in one `read_exact` (rather than
/// byte-by-byte scanning) avoids Android's accessory-fd small-read latency.
/// Keep in sync with `host::SYNC_PREAMBLE`.
const SYNC_MAGIC: &[u8] = b"FERRITE\0";
const PREAMBLE_LEN: usize = 511;

// -----------------------------------------------------------------------------
// Shared: write half published so sendPointer/sendTouches can push upstream
// without opening a second connection. Set to `Some` between the Hello write
// and the protocol loop exiting; cleared on scope exit via `ClearOnDrop`.

fn tx_writer() -> &'static Mutex<Option<Box<dyn Write + Send>>> {
    static S: OnceLock<Mutex<Option<Box<dyn Write + Send>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// Raw fd of the active AOA accessory, tracked so `disconnect()` can
/// `shutdown(2)` it from another thread — that unblocks the protocol
/// reader's `read_exact`, which a simple `File::drop` on the writer half
/// can't. -1 when no AOA session is active.
static AOA_FD: AtomicI32 = AtomicI32::new(-1);

// -----------------------------------------------------------------------------
// Legacy health check kept around for the Kotlin side's `connect()` call.

#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_connect(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    env.new_string("ferrite-android ready").unwrap().into_raw()
}

// -----------------------------------------------------------------------------
// TCP transport (Wi-Fi or adb-reverse localhost).

/// Opens a TCP connection to `host:port` and hands it to the shared protocol
/// loop. Blocks the caller until the connection errors or the callback
/// throws. Throws `java.lang.RuntimeException` on any I/O or protocol error.
#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_streamTcp<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    host: JString<'l>,
    port: jint,
    device_name: JString<'l>,
    width: jint,
    height: jint,
    callback: JObject<'l>,
) {
    let host_str: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", format!("bad host: {e}"));
            return;
        }
    };
    let name_str: String = match env.get_string(&device_name) {
        Ok(s) => s.into(),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", format!("bad device_name: {e}"));
            return;
        }
    };
    if let Err(e) = do_stream_tcp(
        &mut env,
        &host_str,
        port as u16,
        &name_str,
        width as u32,
        height as u32,
        &callback,
    ) {
        if !env.exception_check().unwrap_or(false) {
            let _ = env.throw_new("java/lang/RuntimeException", format!("{e:#}"));
        }
    }
}

fn do_stream_tcp(
    env: &mut JNIEnv,
    host: &str,
    port: u16,
    device_name: &str,
    width: u32,
    height: u32,
    callback: &JObject,
) -> anyhow::Result<()> {
    let sock = TcpStream::connect((host, port))?;
    // No read timeout: host may go idle between frames when nothing changes
    // on screen. But enable TCP keepalive so the kernel detects a dead
    // connection (e.g. USB cable yanked) within ~10s instead of the default
    // ~2 hours.
    let sock2 = socket2::SockRef::from(&sock);
    let _ = sock2.set_keepalive(true);
    let _ = sock2.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(5))
            .with_interval(Duration::from_secs(2)),
    );
    let writer = sock.try_clone().map_err(|e| anyhow::anyhow!("try_clone: {e}"))?;
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, sock);
    run_protocol(env, &mut reader, Box::new(writer), device_name, width, height, callback)
}

// -----------------------------------------------------------------------------
// AOA fd transport. Takes ownership of a UNIX fd obtained from
// `UsbManager.openAccessory(...)`. The read and write halves are two dup'd
// Files pointing at the same bidirectional socketpair, so writes from the
// background `tx_writer` path don't interfere with the protocol reader.

#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_streamFd<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    fd: jint,
    device_name: JString<'l>,
    width: jint,
    height: jint,
    callback: JObject<'l>,
) {
    let name_str: String = match env.get_string(&device_name) {
        Ok(s) => s.into(),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", format!("bad device_name: {e}"));
            return;
        }
    };
    if let Err(e) = do_stream_fd(&mut env, fd, &name_str, width as u32, height as u32, &callback) {
        if !env.exception_check().unwrap_or(false) {
            let _ = env.throw_new("java/lang/RuntimeException", format!("{e:#}"));
        }
    }
}

fn do_stream_fd(
    env: &mut JNIEnv,
    fd: jint,
    device_name: &str,
    width: u32,
    height: u32,
    callback: &JObject,
) -> anyhow::Result<()> {
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    let writer_fd = owned.try_clone()?;
    // BufReader is load-bearing for AOA: Android's f_accessory driver gives
    // each read(2) AT MOST ONE USB bulk transfer and DISCARDS remaining bytes
    // when the caller asked for fewer bytes than the transfer contained. Our
    // protocol reads a 4-byte length prefix then N bytes of body; if the host
    // coalesced those into one USB transfer, `read_exact(4)` would drop the
    // body. BufReader issues one large read up-front, capturing the whole
    // transfer into user memory where split reads can drain it safely.
    let reader_file = std::fs::File::from(owned);
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, reader_file);
    let writer = std::fs::File::from(writer_fd);
    AOA_FD.store(fd, Ordering::Relaxed);
    struct ClearFd;
    impl Drop for ClearFd {
        fn drop(&mut self) {
            AOA_FD.store(-1, Ordering::Relaxed);
        }
    }
    let _fd_guard = ClearFd;
    run_protocol(env, &mut reader, Box::new(writer), device_name, width, height, callback)
}

// -----------------------------------------------------------------------------
// Shared protocol loop. Transport-agnostic — any `Read + Write` works.
//
// Flow:
//   1. Drain stale bytes until SYNC_MAGIC received from host.
//   2. Write Hello (device_name + dimensions).
//   3. Publish a clone of the write half as `tx_writer` so sendPointer /
//      sendTouches can push upstream without racing Hello.
//   4. Loop reading length-prefixed `HostMessage`s, handing frames to the
//      Kotlin callback.

fn run_protocol<R: Read>(
    env: &mut JNIEnv,
    reader: &mut R,
    writer: Box<dyn Write + Send>,
    device_name: &str,
    width: u32,
    height: u32,
    callback: &JObject,
) -> anyhow::Result<()> {
    // Read exactly PREAMBLE_LEN bytes and look for the magic. If there's
    // stale data on the wire, the magic won't be at offset 0; we scan for
    // it, then read exactly as many additional bytes as needed to make our
    // position line up with the end of the preamble on the host side. No
    // over-read — bytes after the preamble belong to real protocol frames
    // and must stay on the wire for the next read_frame call.
    let mut buf = vec![0u8; PREAMBLE_LEN];
    reader.read_exact(&mut buf)?;
    let stale_bytes = loop {
        if let Some(pos) = memmem(&buf, SYNC_MAGIC) {
            break pos;
        }
        // Magic not found in current window. It may have been split across
        // boundaries or be further into the stream; slide the window.
        // Keep the tail that could be a prefix of the magic.
        let keep = SYNC_MAGIC.len() - 1;
        let shift = buf.len() - keep;
        buf.copy_within(shift.., 0);
        reader.read_exact(&mut buf[keep..])?;
        // Total stale bytes so far (for bail-out).
        if buf.len() > 64 * 1024 {
            anyhow::bail!("no sync magic in first 64K of stream");
        }
    };
    // Preamble = SYNC_MAGIC (8) + FILLER (PREAMBLE_LEN-8). buf currently has
    // `stale_bytes + PREAMBLE_LEN - stale_bytes = PREAMBLE_LEN` bytes, of
    // which the first `stale_bytes` were junk and the next 8 were magic.
    // We've read `PREAMBLE_LEN - stale_bytes - 8` filler bytes already;
    // need `stale_bytes` more filler bytes to finish the preamble.
    if stale_bytes > 0 {
        let mut rest = vec![0u8; stale_bytes];
        reader.read_exact(&mut rest)?;
    }
    android_log(&format!("sync magic received (stale={stale_bytes})"));

    // Write Hello via `writer` before publishing it. Do it in-place through a
    // small owned handle; after that the boxed writer goes into tx_writer.
    let mut w = writer;
    let hello = bincode::serialize(&ClientMessage::Hello {
        device_name: device_name.to_string(),
        width,
        height,
    })?;
    write_frame(&mut *w, &hello)?;

    *tx_writer().lock().unwrap() = Some(w);
    struct ClearOnDrop;
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            *tx_writer().lock().unwrap() = None;
        }
    }
    let _guard = ClearOnDrop;

    read_loop(env, reader, callback)
}

fn read_loop<R: Read>(
    env: &mut JNIEnv,
    reader: &mut R,
    callback: &JObject,
) -> anyhow::Result<()> {
    let mut first = true;
    loop {
        let buf = read_frame(reader)?;
        if first {
            let head: Vec<String> = buf.iter().take(16).map(|b| format!("{b:02x}")).collect();
            android_log(&format!("first frame: len={} head=[{}]", buf.len(), head.join(" ")));
            first = false;
        }
        let msg: HostMessage = bincode::deserialize(&buf)?;
        let (fmt, data, width, height) = match msg {
            HostMessage::VideoFrame {
                format,
                data,
                width,
                height,
            } => (format, data, width, height),
            HostMessage::Ping => {
                let pong = bincode::serialize(&ClientMessage::Pong)?;
                // Pong goes through tx_writer so the reader loop isn't
                // blocked on acquiring the writer.
                if let Some(w) = tx_writer().lock().unwrap().as_mut() {
                    write_frame(&mut **w, &pong)?;
                }
                continue;
            }
        };
        let format_id: i32 = match fmt {
            PixelFormat::Rgba8 => {
                let expected = (width as usize) * (height as usize) * 4;
                if data.len() != expected {
                    anyhow::bail!(
                        "rgba frame size mismatch: got {} bytes, expected {expected}",
                        data.len()
                    );
                }
                0
            }
            PixelFormat::Jpeg => 1,
            PixelFormat::H264 => 2,
        };

        env.with_local_frame::<_, _, anyhow::Error>(4, |env| {
            let arr: JByteArray = env.byte_array_from_slice(&data)?;
            env.call_method(
                callback,
                "onFrame",
                "([BIII)V",
                &[
                    JValue::Object(&arr),
                    JValue::Int(width as i32),
                    JValue::Int(height as i32),
                    JValue::Int(format_id),
                ],
            )?;
            if env.exception_check()? {
                env.exception_describe()?;
                env.exception_clear()?;
                anyhow::bail!("onFrame threw");
            }
            Ok(())
        })?;
    }
}

// -----------------------------------------------------------------------------
// Upstream writers (touch + pointer) — go through `tx_writer` regardless of
// transport.

#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_sendTouches<'l>(
    env: JNIEnv<'l>,
    _class: JClass<'l>,
    ids: JIntArray<'l>,
    xs: JFloatArray<'l>,
    ys: JFloatArray<'l>,
) {
    let n = match env.get_array_length(&ids) {
        Ok(n) => n as usize,
        Err(_) => return,
    };
    let mut id_buf = vec![0i32; n];
    let mut x_buf = vec![0f32; n];
    let mut y_buf = vec![0f32; n];
    if n > 0 {
        if env.get_int_array_region(&ids, 0, &mut id_buf).is_err() {
            return;
        }
        if env.get_float_array_region(&xs, 0, &mut x_buf).is_err() {
            return;
        }
        if env.get_float_array_region(&ys, 0, &mut y_buf).is_err() {
            return;
        }
    }
    let points: Vec<TouchPoint> = (0..n)
        .map(|i| TouchPoint {
            id: id_buf[i] as u32,
            x: x_buf[i],
            y: y_buf[i],
        })
        .collect();
    send_upstream(&ClientMessage::Touches { points });
}

/// `tool`: 0 = Finger, 1 = Pen, 2 = Eraser. Anything else is treated as Finger.
#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_sendPointer(
    _env: JNIEnv,
    _class: JClass,
    x: jfloat,
    y: jfloat,
    pressed: jboolean,
    pressure: jfloat,
    tool: jint,
    in_range: jboolean,
) {
    let tool = match tool {
        1 => PointerTool::Pen,
        2 => PointerTool::Eraser,
        _ => PointerTool::Finger,
    };
    send_upstream(&ClientMessage::Pointer {
        x: x as f32,
        y: y as f32,
        pressed: pressed != 0,
        pressure: pressure as f32,
        tool,
        in_range: in_range != 0,
    });
}

fn send_upstream(msg: &ClientMessage) {
    let bytes = match bincode::serialize(msg) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut guard = tx_writer().lock().unwrap();
    if let Some(w) = guard.as_mut() {
        let len = (bytes.len() as u32).to_be_bytes();
        if w.write_all(&len).is_err() || w.write_all(&bytes).is_err() {
            *guard = None;
        }
    }
}

/// Drop the writer and shut down any active AOA fd. Either unblocks the
/// blocking protocol reader so the stream call returns with an I/O error,
/// letting the caller start a fresh session. Dropping the writer alone
/// isn't enough for AOA: reader and writer are dup'd fds pointing at the
/// same socketpair end, so the reader's `read_exact` stays blocked until we
/// `shutdown(2)` the socket.
#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_disconnect(
    _env: JNIEnv,
    _class: JClass,
) {
    tx_writer().lock().unwrap().take();
    let fd = AOA_FD.swap(-1, Ordering::Relaxed);
    if fd >= 0 {
        unsafe {
            libc::shutdown(fd, libc::SHUT_RDWR);
        }
    }
}

/// Minimal needle-in-haystack byte search. std's slice::contains doesn't help
/// because we need the starting index, not just presence.
fn memmem(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

// -----------------------------------------------------------------------------
// Framing helpers.

fn read_frame<R: Read + ?Sized>(sock: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    sock.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; len];
    sock.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_frame<W: Write + ?Sized>(sock: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    sock.write_all(&(bytes.len() as u32).to_be_bytes())?;
    sock.write_all(bytes)?;
    Ok(())
}
