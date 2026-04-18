//! Ferrite tray: the one always-running process.
//!
//! - Owns the `ferrite-host` child: starts it, restarts it on unexpected
//!   exit, stops it on mode change / disable / quit.
//! - Publishes a `StatusNotifierItem` tray icon with menu + tooltip.
//! - Spawns `ferrite-ui` on demand — each invocation is a throwaway window.
//!
//! Expected to be autostarted at login (see `packaging/`).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use ferrite_core::{Status, status_path};
use ksni::blocking::{Handle, TrayMethods};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Mode {
    Mirror,
    Virtual,
}

impl Mode {
    fn env(self) -> &'static str {
        match self {
            Mode::Mirror => "mirror",
            Mode::Virtual => "virtual",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Config {
    #[serde(default)]
    mode: Option<Mode>,
    #[serde(default)]
    enabled: Option<bool>,
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("ferrite").join("tray.ron"))
}

fn load_config() -> Config {
    let Some(p) = config_path() else {
        return Config::default();
    };
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| ron::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_config(cfg: &Config) {
    let Some(p) = config_path() else { return };
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = ron::ser::to_string_pretty(cfg, ron::ser::PrettyConfig::default()) {
        let _ = std::fs::write(p, s);
    }
}

/// Locate sibling binaries in the same directory as this executable; that's
/// how cargo lays things out, and how the `packaging/install.sh` script
/// deploys a release bundle.
fn sibling_exe(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .map(|d| d.join(name))
        .unwrap_or_else(|| PathBuf::from(name))
}

// -----------------------------------------------------------------------------
// Host manager: owns the ferrite-host child. Runs on a dedicated thread,
// driven by a command channel. Responsibilities:
//
//   1. Start / stop the child on explicit commands from the tray menu.
//   2. Restart the child on unexpected exit (backoff 1s, capped 30s).
//   3. Poll $XDG_RUNTIME_DIR/ferrite-status.json and push the connected
//      client count back into the tray via the ksni handle.

enum Cmd {
    Start(Mode),
    Stop,
    Quit,
}

fn spawn_host(mode: Mode) -> std::io::Result<Child> {
    let exe = sibling_exe("ferrite-host");
    tracing::info!(mode = ?mode, exe = %exe.display(), "starting host");
    let mut cmd = Command::new(&exe);
    cmd.env("FERRITE_MODE", mode.env());
    // Default RUST_LOG=info so spawned host logs land in the tray's journal
    // entry (or wherever stderr goes) — users can override by setting
    // RUST_LOG before launching the tray.
    if std::env::var_os("RUST_LOG").is_none() {
        cmd.env("RUST_LOG", "info");
    }
    cmd.stdout(Stdio::null()).stderr(Stdio::inherit());
    cmd.spawn()
}

fn manager_loop(rx: mpsc::Receiver<Cmd>, handle: Handle<Ferrite>) {
    let mut active_mode: Option<Mode> = None;
    let mut child: Option<Child> = None;
    let mut backoff = Duration::from_secs(1);

    loop {
        let tick = Duration::from_millis(500);
        match rx.recv_timeout(tick) {
            Ok(Cmd::Start(mode)) => {
                kill(&mut child);
                active_mode = Some(mode);
                backoff = Duration::from_secs(1);
                match spawn_host(mode) {
                    Ok(c) => child = Some(c),
                    Err(e) => tracing::warn!(error = %e, "host spawn failed"),
                }
            }
            Ok(Cmd::Stop) => {
                active_mode = None;
                kill(&mut child);
            }
            Ok(Cmd::Quit) => {
                kill(&mut child);
                return;
            }
            Err(RecvTimeoutError::Timeout) => {
                // Poll child status + refresh tooltip.
                if let Some(c) = child.as_mut() {
                    match c.try_wait() {
                        Ok(Some(status)) => {
                            tracing::warn!(
                                ?status,
                                backoff_s = backoff.as_secs(),
                                "host exited, restarting after backoff"
                            );
                            child = None;
                            thread::sleep(backoff);
                            backoff = (backoff * 2).min(Duration::from_secs(30));
                            if let Some(mode) = active_mode {
                                match spawn_host(mode) {
                                    Ok(c) => child = Some(c),
                                    Err(e) => {
                                        tracing::warn!(error = %e, "host respawn failed")
                                    }
                                }
                            }
                        }
                        Ok(None) => {
                            // Still running; reset backoff once we've survived
                            // a tick without exit.
                            backoff = Duration::from_secs(1);
                        }
                        Err(e) => tracing::warn!(error = %e, "try_wait failed"),
                    }
                }
                refresh_tooltip(&handle, child.is_some());
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

fn kill(child: &mut Option<Child>) {
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }
}

fn read_status() -> Option<Status> {
    let p = status_path();
    let s = std::fs::read_to_string(p).ok()?;
    serde_json::from_str(&s).ok()
}

fn refresh_tooltip(handle: &Handle<Ferrite>, running: bool) {
    let clients = if running {
        read_status().map(|s| s.clients.len()).unwrap_or(0)
    } else {
        0
    };
    handle.update(move |t| {
        t.running = running;
        t.clients = clients;
    });
}

// -----------------------------------------------------------------------------
// Tray UI. `tx` is the command channel into the host manager; `running` and
// `clients` are refreshed from the manager every ~500ms via handle.update.

struct Ferrite {
    tx: Sender<Cmd>,
    mode: Mode,
    enabled: bool,
    running: bool,
    clients: usize,
}

impl ksni::Tray for Ferrite {
    fn id(&self) -> String {
        "co.dealdrive.Ferrite".into()
    }
    fn title(&self) -> String {
        "Ferrite".into()
    }
    fn icon_name(&self) -> String {
        // Active glyph when a host process is alive; greyed-out when disabled
        // or while the host has crashed and we haven't restarted it yet.
        if self.enabled && self.running {
            "video-display".into()
        } else {
            "video-display-symbolic".into()
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let state = if !self.enabled {
            "disabled".to_string()
        } else if !self.running {
            "host not running".to_string()
        } else {
            match self.clients {
                0 => "no clients connected".to_string(),
                1 => "1 client connected".to_string(),
                n => format!("{n} clients connected"),
            }
        };
        ksni::ToolTip {
            title: format!("Ferrite — {}", self.mode_label()),
            description: state,
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        open_panel();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let current_mode = self.mode;
        let status = if !self.enabled {
            "disabled".to_string()
        } else if !self.running {
            "host not running".to_string()
        } else {
            match self.clients {
                0 => "waiting for client".to_string(),
                1 => "1 client connected".to_string(),
                n => format!("{n} clients connected"),
            }
        };
        vec![
            StandardItem {
                label: format!("Ferrite ({}) — {status}", self.mode_label()),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Enabled".into(),
                checked: self.enabled,
                activate: Box::new(|this: &mut Self| this.set_enabled(!this.enabled)),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Open Panel".into(),
                activate: Box::new(|_: &mut Self| open_panel()),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "Mode".into(),
                submenu: vec![
                    CheckmarkItem {
                        label: "Virtual monitor".into(),
                        checked: current_mode == Mode::Virtual,
                        activate: Box::new(|this: &mut Self| this.set_mode(Mode::Virtual)),
                        ..Default::default()
                    }
                    .into(),
                    CheckmarkItem {
                        label: "Mirror".into(),
                        checked: current_mode == Mode::Mirror,
                        activate: Box::new(|this: &mut Self| this.set_mode(Mode::Mirror)),
                        ..Default::default()
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|this: &mut Self| {
                    let _ = this.tx.send(Cmd::Quit);
                    // Give the manager a beat to SIGTERM the child before we exit.
                    thread::sleep(Duration::from_millis(200));
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

impl Ferrite {
    fn mode_label(&self) -> &'static str {
        match self.mode {
            Mode::Mirror => "mirror",
            Mode::Virtual => "virtual monitor",
        }
    }

    fn set_mode(&mut self, mode: Mode) {
        if mode == self.mode {
            return;
        }
        self.mode = mode;
        save_config(&Config {
            mode: Some(mode),
            enabled: Some(self.enabled),
        });
        if self.enabled {
            let _ = self.tx.send(Cmd::Start(mode));
        }
    }

    fn set_enabled(&mut self, enabled: bool) {
        if enabled == self.enabled {
            return;
        }
        self.enabled = enabled;
        save_config(&Config {
            mode: Some(self.mode),
            enabled: Some(enabled),
        });
        if enabled {
            let _ = self.tx.send(Cmd::Start(self.mode));
        } else {
            let _ = self.tx.send(Cmd::Stop);
        }
    }
}

fn open_panel() {
    let exe = sibling_exe("ferrite-ui");
    tracing::info!(exe = %exe.display(), "opening panel");
    if let Err(e) = Command::new(&exe)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        tracing::warn!(error = %e, "failed to spawn ferrite-ui");
    }
}

fn main() {
    tracing_subscriber::fmt::init();

    let cfg = load_config();
    let mode = cfg.mode.unwrap_or(Mode::Virtual);
    let enabled = cfg.enabled.unwrap_or(true);

    let (tx, rx) = mpsc::channel::<Cmd>();

    let tray = Ferrite {
        tx: tx.clone(),
        mode,
        enabled,
        running: false,
        clients: 0,
    };
    let handle = tray.spawn().expect("spawn tray service");

    // Kick off the host manager. It'll pick up the initial Start below.
    let mgr_handle = handle.clone();
    thread::spawn(move || manager_loop(rx, mgr_handle));

    if enabled {
        let _ = tx.send(Cmd::Start(mode));
    }

    // Keep the ksni handle alive for the lifetime of main — it owns the
    // D-Bus registration. The Quit menu calls std::process::exit.
    let _handle = handle;
    loop {
        thread::park();
    }
}
