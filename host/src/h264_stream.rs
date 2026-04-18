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
            // CQP avoids the rate-control feedback loop's frame buffering, so
            // the encoder's pipeline depth bottoms out. Bitrate becomes variable
            // (spikes on motion) but we'd rather pay bandwidth than ms.
            "-rc_mode",
            "CQP",
            "-qp",
            "22",
            "-quality",
            "7", // fastest VAAPI preset (1=best quality, 7=fastest)
            "-g",
            &(fps / 2).max(1).to_string(), // IDR every ~0.5s for fast recovery
            "-bf",
            "0",
            "-async_depth",
            "1",
            "-aud",
            "1", // emit AUD NAL so we can split the stream into AUs cleanly
            "-bsf:v",
            // Prepend SPS/PPS to every IDR so the decoder can resync without
            // waiting for a full GOP cycle if it loses parameter state.
            "dump_extra=freq=keyframe",
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
