//! Screen capture via xdg-desktop-portal ScreenCast + PipeWire.
//!
//! `start()` does the portal handshake on the caller's tokio runtime, then spawns
//! a dedicated OS thread running the blocking libpipewire mainloop. Each incoming
//! frame is converted from whatever format PipeWire negotiated into tightly-packed
//! R,G,B bytes and published on a `watch` channel. Encoders (per client) consume
//! that RGB stream.

use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, anyhow};
use ashpd::desktop::PersistMode;
use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream as ScStream,
};
use pipewire as pw;
use pw::spa;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::{properties::properties, stream::StreamFlags};
use tokio::sync::watch;
use tracing::{debug, error, info, warn};

use crate::h264_dump::H264Dump;

pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>, // tightly packed, row-major, 3 bytes/pixel
}

pub type FrameTx = watch::Sender<Option<Arc<Frame>>>;
pub type FrameRx = watch::Receiver<Option<Arc<Frame>>>;

pub async fn start(tx: FrameTx) -> Result<()> {
    let (stream_info, fd) = open_portal().await.context("portal handshake")?;
    let node_id = stream_info.pipe_wire_node_id();
    info!(node_id, "portal granted screencast");

    thread::Builder::new()
        .name("pipewire-capture".into())
        .spawn(move || {
            if let Err(e) = run_pipewire(fd, node_id, tx) {
                error!(error = %e, "pipewire thread exited");
            }
        })
        .context("spawn pipewire thread")?;
    Ok(())
}

async fn open_portal() -> Result<(ScStream, OwnedFd)> {
    let proxy = Screencast::new().await.context("Screencast::new")?;
    let session = proxy
        .create_session(Default::default())
        .await
        .context("create_session")?;

    let cached_token = load_portal_token();
    if cached_token.is_some() {
        info!("re-using cached portal restore_token");
    }

    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(SourceType::Monitor | SourceType::Window)
                .set_multiple(false)
                .set_restore_token(cached_token.as_deref())
                .set_persist_mode(PersistMode::Application),
        )
        .await
        .context("select_sources")?;

    let response = proxy
        .start(&session, None, Default::default())
        .await
        .context("start")?
        .response()
        .context("start response")?;

    if let Some(tok) = response.restore_token() {
        save_portal_token(tok);
    }

    let stream_info = response
        .streams()
        .first()
        .ok_or_else(|| anyhow!("portal returned no streams"))?
        .to_owned();

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .context("open_pipe_wire_remote")?;

    Ok((stream_info, fd))
}

fn portal_token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("ferrite").join("portal_token"))
}

fn load_portal_token() -> Option<String> {
    let p = portal_token_path()?;
    let s = std::fs::read_to_string(&p).ok()?;
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

fn save_portal_token(token: &str) {
    let Some(p) = portal_token_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&p, token) {
        warn!(path = %p.display(), error = %e, "could not save portal token");
    } else {
        info!(path = %p.display(), "saved portal restore_token");
    }
}

struct UserData {
    format: VideoInfoRaw,
    tx: FrameTx,
    h264_dump: Option<H264Dump>,
    h264_dump_path: Option<PathBuf>,
}

fn run_pipewire(fd: OwnedFd, node_id: u32, tx: FrameTx) -> Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopBox::new(None).context("MainLoopBox::new")?;
    let context =
        pw::context::ContextBox::new(mainloop.loop_(), None).context("ContextBox::new")?;
    let core = context.connect_fd(fd, None).context("connect_fd")?;

    let h264_dump_path = std::env::var_os("FERRITE_H264_DUMP").map(PathBuf::from);
    if let Some(p) = &h264_dump_path {
        info!(path = %p.display(), "H264 dump requested via FERRITE_H264_DUMP");
    }

    let data = UserData {
        format: VideoInfoRaw::default(),
        tx,
        h264_dump: None,
        h264_dump_path,
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "ferrite-capture",
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .context("StreamBox::new")?;

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed(|_, _, old, new| {
            info!(?old, ?new, "pipewire stream state");
        })
        .param_changed(|_, user_data, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let parsed = pw::spa::param::format_utils::parse_format(param);
            let Ok((mt, mst)) = parsed else { return };
            if mt != pw::spa::param::format::MediaType::Video
                || mst != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if let Err(e) = user_data.format.parse(param) {
                warn!(error = ?e, "parse VideoInfoRaw");
                return;
            }
            let f = &user_data.format;
            info!(
                format = ?f.format(),
                w = f.size().width, h = f.size().height,
                fps_num = f.framerate().num, fps_denom = f.framerate().denom,
                "negotiated video format"
            );
        })
        .process(|stream, user_data: &mut UserData| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                debug!("out of buffers");
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let d = &mut datas[0];
            let chunk = d.chunk();
            let stride = chunk.stride() as usize;
            let Some(src) = d.data() else { return };

            let w = user_data.format.size().width;
            let h = user_data.format.size().height;
            if w == 0 || h == 0 {
                return;
            }

            let fmt = user_data.format.format();
            let mut rgb = vec![0u8; w as usize * h as usize * 3];
            if !convert_to_rgb(src, w as usize, h as usize, stride, fmt, &mut rgb) {
                warn!(?fmt, "unsupported pixel format");
                return;
            }

            if user_data.h264_dump.is_none() {
                if let Some(path) = user_data.h264_dump_path.clone() {
                    match H264Dump::new(w, h, 30, &path) {
                        Ok(d) => user_data.h264_dump = Some(d),
                        Err(e) => {
                            warn!(error = %e, "H264 dump init failed; disabling");
                            user_data.h264_dump_path = None;
                        }
                    }
                }
            }
            if let Some(d) = user_data.h264_dump.as_mut() {
                d.write_rgb(&rgb);
            }

            let _ = user_data.tx.send(Some(Arc::new(Frame {
                width: w,
                height: h,
                rgb,
            })));
        })
        .register()
        .context("register listener")?;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: 1920,
                height: 1080
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 7680,
                height: 4320
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 30, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction {
                num: 120,
                denom: 1
            }
        ),
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .context("serialize format pod")?
    .0
    .into_inner();
    let mut params = [spa::pod::Pod::from_bytes(&values).context("pod from_bytes")?];

    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .context("stream connect")?;

    info!("pipewire mainloop running");
    mainloop.run();
    Ok(())
}

/// Pack `src` (pipewire-negotiated format, possibly padded via `stride`) into
/// tightly-packed R,G,B bytes. Returns false for unsupported formats.
fn convert_to_rgb(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    fmt: VideoFormat,
    dst: &mut [u8],
) -> bool {
    let dst_stride = w * 3;
    match fmt {
        VideoFormat::BGRx | VideoFormat::BGRA => {
            for y in 0..h {
                let row_range = y * stride..y * stride + w * 4;
                let Some(row) = src.get(row_range) else { return false };
                let drow = &mut dst[y * dst_stride..y * dst_stride + dst_stride];
                for x in 0..w {
                    let p = &row[x * 4..x * 4 + 4];
                    let d = &mut drow[x * 3..x * 3 + 3];
                    d[0] = p[2];
                    d[1] = p[1];
                    d[2] = p[0];
                }
            }
        }
        VideoFormat::RGBx | VideoFormat::RGBA => {
            for y in 0..h {
                let row_range = y * stride..y * stride + w * 4;
                let Some(row) = src.get(row_range) else { return false };
                let drow = &mut dst[y * dst_stride..y * dst_stride + dst_stride];
                for x in 0..w {
                    let p = &row[x * 4..x * 4 + 4];
                    let d = &mut drow[x * 3..x * 3 + 3];
                    d[0] = p[0];
                    d[1] = p[1];
                    d[2] = p[2];
                }
            }
        }
        _ => return false,
    }
    true
}
