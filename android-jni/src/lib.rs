use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use ferrite_core::{ClientMessage, HostMessage};
use jni::JNIEnv;
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};

#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_connect(
    env: JNIEnv,
    _class: JClass,
) -> jstring {
    env.new_string("ferrite-android ready").unwrap().into_raw()
}

#[no_mangle]
pub extern "system" fn Java_com_ferrite_FerriteLib_ping(
    mut env: JNIEnv,
    _class: JClass,
    host: JString,
    port: jint,
) -> jstring {
    let host: String = match env.get_string(&host) {
        Ok(s) => s.into(),
        Err(e) => return mk_string(&mut env, &format!("err: bad host string: {e}")),
    };
    let msg = match do_ping(&host, port as u16) {
        Ok(s) => s,
        Err(e) => format!("err: {e:#}"),
    };
    mk_string(&mut env, &msg)
}

fn mk_string(env: &mut JNIEnv, s: &str) -> jstring {
    env.new_string(s).unwrap().into_raw()
}

fn do_ping(host: &str, port: u16) -> anyhow::Result<String> {
    let mut sock = TcpStream::connect((host, port))?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    sock.set_write_timeout(Some(Duration::from_secs(5)))?;

    let buf = read_frame(&mut sock)?;
    let msg: HostMessage = bincode::deserialize(&buf)?;
    if !matches!(msg, HostMessage::Ping) {
        anyhow::bail!("expected Ping, got {msg:?}");
    }

    let out = bincode::serialize(&ClientMessage::Pong)?;
    write_frame(&mut sock, &out)?;

    Ok(format!("ok — ping/pong with {host}:{port}"))
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
