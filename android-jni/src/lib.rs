use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use ferrite_core::{ClientMessage, HostMessage, PixelFormat, PointerTool};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jboolean, jfloat, jint, jstring};

/// Write-half of the active stream socket, shared with `sendTouch` on the
/// Kotlin side so it can push `ClientMessage` upstream without opening a
/// second connection.
fn tx_sock() -> &'static Mutex<Option<TcpStream>> {
    static S: OnceLock<Mutex<Option<TcpStream>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_connect(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    env.new_string("ferrite-android ready").unwrap().into_raw()
}

/// Opens a TCP connection to `host:port`, sends `Hello` (so the host can size
/// its virtual monitor + name our input devices), and then loops forever
/// reading length-prefixed `bincode` `HostMessage::VideoFrame`s, calling
/// `callback.onFrame(byte[], int, int, int)` for each frame. Blocks the caller
/// until the connection errors or the callback throws. Throws
/// `java.lang.RuntimeException` on any I/O / protocol / JNI error.
#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_stream<'l>(
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
    if let Err(e) = do_stream(
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

fn do_stream(
    env: &mut JNIEnv,
    host: &str,
    port: u16,
    device_name: &str,
    width: u32,
    height: u32,
    callback: &JObject,
) -> anyhow::Result<()> {
    let mut sock = TcpStream::connect((host, port))?;
    // No read timeout: host may go idle between frames when nothing changes on
    // screen. But enable TCP keepalive so the kernel detects a dead connection
    // (e.g. USB cable yanked) within ~10s instead of the default ~2 hours.
    let sock2 = socket2::SockRef::from(&sock);
    let _ = sock2.set_keepalive(true);
    let _ = sock2.set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(5))
            .with_interval(Duration::from_secs(2)),
    );

    // Publish a write-half clone so `sendTouch` can push ClientMessages.
    let writer = sock.try_clone().map_err(|e| anyhow::anyhow!("try_clone: {e}"))?;
    *tx_sock().lock().unwrap() = Some(writer);

    // Hello drives host-side monitor sizing + device naming.
    let hello = bincode::serialize(&ClientMessage::Hello {
        device_name: device_name.to_string(),
        width,
        height,
    })?;
    write_frame(&mut sock, &hello)?;
    // On scope exit (loop breaks via ?), clear the slot so stale writes bail.
    struct ClearOnDrop;
    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            *tx_sock().lock().unwrap() = None;
        }
    }
    let _guard = ClearOnDrop;

    loop {
        let buf = read_frame(&mut sock)?;
        let msg: HostMessage = bincode::deserialize(&buf)?;
        let (fmt, data, width, height) = match msg {
            HostMessage::VideoFrame {
                format,
                data,
                width,
                height,
            } => (format, data, width, height),
            HostMessage::Ping => {
                write_frame(&mut sock, &bincode::serialize(&ClientMessage::Pong)?)?;
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

        // Use a fresh local frame so the jbyteArray is freed each iteration.
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

/// Force-close the active stream socket. Causes the blocking `stream()` JNI
/// call to unwind via I/O error so the caller can start a new connection
/// without waiting for the host to time out.
#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_disconnect(
    _env: JNIEnv,
    _class: JClass,
) {
    let mut g = tx_sock().lock().unwrap();
    if let Some(s) = g.take() {
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
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
    let msg = ClientMessage::Pointer {
        x: x as f32,
        y: y as f32,
        pressed: pressed != 0,
        pressure: pressure as f32,
        tool,
        in_range: in_range != 0,
    };
    let bytes = match bincode::serialize(&msg) {
        Ok(b) => b,
        Err(_) => return,
    };
    let mut guard = tx_sock().lock().unwrap();
    if let Some(sock) = guard.as_mut() {
        let len = (bytes.len() as u32).to_be_bytes();
        if sock.write_all(&len).is_err() || sock.write_all(&bytes).is_err() {
            *guard = None;
        }
    }
}

fn read_frame(sock: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    sock.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    let mut buf = vec![0u8; len];
    sock.read_exact(&mut buf)?;
    Ok(buf)
}

fn write_frame(sock: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    sock.write_all(&(bytes.len() as u32).to_be_bytes())?;
    sock.write_all(bytes)?;
    Ok(())
}
