//! Diagnostic-only H.264 file dump via `ffmpeg` subprocess + VA-API.
//!
//! When the `FERRITE_H264_DUMP` env var names a file path, the capture thread
//! spawns ffmpeg once the PipeWire stream's dimensions are known and pipes
//! tight RGB frames into its stdin. ffmpeg writes Annex-B H.264 to the target
//! file. Play back with `ffplay <path>`.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use anyhow::{Context, Result};
use tracing::{info, warn};

pub struct H264Dump {
    child: Child,
    stdin: Option<ChildStdin>,
    path: PathBuf,
}

impl H264Dump {
    pub fn new(width: u32, height: u32, fps: u32, path: &Path) -> Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-y",
            "-loglevel",
            "warning",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "-s",
            &format!("{}x{}", width, height),
            "-framerate",
            &fps.to_string(),
            "-i",
            "pipe:0",
            "-vaapi_device",
            "/dev/dri/renderD128",
            "-vf",
            "format=nv12,hwupload",
            "-c:v",
            "h264_vaapi",
            "-b:v",
            "8M",
            "-g",
            "60",
            path.to_str().context("non-utf8 output path")?,
        ]);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::inherit());

        let mut child = cmd.spawn().context("spawn ffmpeg (is ffmpeg installed?)")?;
        let stdin = child.stdin.take().context("take ffmpeg stdin")?;

        info!(
            width,
            height,
            fps,
            path = %path.display(),
            "H264 dump: ffmpeg started"
        );

        Ok(Self {
            child,
            stdin: Some(stdin),
            path: path.to_path_buf(),
        })
    }

    pub fn write_rgb(&mut self, rgb: &[u8]) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };
        if let Err(e) = stdin.write_all(rgb) {
            warn!(error = %e, "H264 dump: stdin write failed; disabling");
            self.stdin = None;
        }
    }
}

impl Drop for H264Dump {
    fn drop(&mut self) {
        // Closing stdin signals EOF; ffmpeg then flushes and exits.
        drop(self.stdin.take());
        match self.child.wait() {
            Ok(status) => info!(?status, path = %self.path.display(), "H264 dump: ffmpeg exited"),
            Err(e) => warn!(error = %e, "H264 dump: wait() failed"),
        }
    }
}
