use std::io::{Read, Write};
use std::net::TcpStream;

use ferrite_core::{ClientMessage, HostMessage, PixelFormat};
use jni::JNIEnv;
use jni::objects::{JByteArray, JClass, JObject, JString, JValue};
use jni::sys::{jint, jstring};

#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_connect(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    env.new_string("ferrite-android ready").unwrap().into_raw()
}

/// Opens a TCP connection to `host:port` and then loops forever, reading length-
/// prefixed `bincode` `HostMessage::VideoFrame`s and calling `callback.onFrame(
/// byte[], int, int)` for each frame. Blocks the caller until the connection
/// errors or the callback throws. Throws `java.lang.RuntimeException` on any
/// I/O / protocol / JNI error.
#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_stream<'l>(
    mut env: JNIEnv<'l>,
    _class: JClass<'l>,
    host: JString<'l>,
    port: jint,
    callback: JObject<'l>,
) {
    let host_str: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(e) => {
            let _ = env.throw_new("java/lang/RuntimeException", format!("bad host: {e}"));
            return;
        }
    };
    if let Err(e) = do_stream(&mut env, &host_str, port as u16, &callback) {
        if !env.exception_check().unwrap_or(false) {
            let _ = env.throw_new("java/lang/RuntimeException", format!("{e:#}"));
        }
    }
}

fn do_stream(
    env: &mut JNIEnv,
    host: &str,
    port: u16,
    callback: &JObject,
) -> anyhow::Result<()> {
    let mut sock = TcpStream::connect((host, port))?;
    // No read timeout: host may go idle between frames when nothing changes on screen.

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
