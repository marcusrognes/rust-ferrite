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

/// First bytes of the preamble the host sends at session start. The client
/// scans for these 8 bytes, discarding any stale data before them, then
/// consumes [`PREAMBLE_FILLER_LEN`] more bytes to align with the host's
/// full write before bincode parsing begins. Keep in sync with
/// `host::SYNC_MAGIC` / `host::SYNC_PREAMBLE`.
const SYNC_MAGIC: &[u8] = b"FERRITE\0";
const PREAMBLE_FILLER_LEN: usize = 503; // 511 total − 8 magic

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
    let mut sock = TcpStream::connect((host, port))?;
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
    run_protocol(env, &mut sock, Box::new(writer), device_name, width, height, callback)
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
    let mut reader = std::fs::File::from(owned);
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
    drain_to_magic(reader)?;
    // Consume the rest of the host's preamble padding so our first frame
    // read doesn't re-interpret filler bytes as a length prefix.
    let mut filler = vec![0u8; PREAMBLE_FILLER_LEN];
    reader.read_exact(&mut filler)?;
    android_log("sync magic received");

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
    loop {
        let buf = read_frame(reader)?;
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

/// Scan the stream for [`SYNC_MAGIC`], discarding everything before the match.
fn drain_to_magic<R: Read>(sock: &mut R) -> std::io::Result<()> {
    let mut matched = 0;
    let mut byte = [0u8; 1];
    while matched < SYNC_MAGIC.len() {
        sock.read_exact(&mut byte)?;
        if byte[0] == SYNC_MAGIC[matched] {
            matched += 1;
        } else if byte[0] == SYNC_MAGIC[0] {
            matched = 1;
        } else {
            matched = 0;
        }
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// Upstream writers (touch + pointer) — go through `tx_writer` regardless of
// transport.

#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_sendTouches<'l>(
    mut env: JNIEnv<'l>,
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
