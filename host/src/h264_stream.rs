//! Per-client H.264 encoder over `ffmpeg` subprocess + VA-API.
//!
//! `spawn()` creates an ffmpeg process tuned for low-latency H.264 encoding
//! (no B-frames, short GOP). The caller writes tight RGB frames to `stdin` and
//! reads H.264 Annex-B bytes from `stdout`. On drop, the process is SIGKILLed.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub struct H264Encoder {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<ChildStdout>,
}

impl H264Encoder {
    pub fn spawn(width: u32, height: u32, fps: u32) -> Result<Self> {
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
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
            "-bf",
            "0",
            "-f",
            "h264",
            "pipe:1",
        ]);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::inherit());
        cmd.kill_on_drop(true);

        let mut child = cmd.spawn().context("spawn ffmpeg (is it installed?)")?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;

        Ok(Self {
            child: child,
            stdin: Some(stdin),
            stdout: Some(stdout),
        })
    }

    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.stdin.take()
    }
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        // `kill_on_drop(true)` above handles it, but be explicit.
        let _ = self.child.start_kill();
    }
}
