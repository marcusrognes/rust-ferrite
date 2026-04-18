//! Ferrite tray: the one always-running process.
//!
//! - Owns the `ferrite-host` child (restarts it on mode change, kills it on quit).
//! - Publishes a `StatusNotifierItem` tray icon with menu for Open Panel /
//!   Mode / Quit.
//! - Spawns `ferrite-ui` on demand — each invocation is a throwaway window.
//!
//! Expected to be autostarted at login (desktop entry / systemd user service).

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use ksni::blocking::TrayMethods;
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
/// how cargo lays things out, and how we'd ship a release bundle.
fn sibling_exe(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .map(|d| d.join(name))
        .unwrap_or_else(|| PathBuf::from(name))
}

struct Host {
    child: Option<Child>,
}

impl Host {
    fn new() -> Self {
        Self { child: None }
    }

    fn start(&mut self, mode: Mode) {
        self.stop();
        let exe = sibling_exe("ferrite-host");
        tracing::info!(mode = ?mode, exe = %exe.display(), "starting host");
        match Command::new(&exe)
            .env("FERRITE_MODE", mode.env())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(c) => self.child = Some(c),
            Err(e) => tracing::warn!(error = %e, "host spawn failed"),
        }
    }

    fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Ferrite {
    host: Arc<Mutex<Host>>,
    mode: Mode,
    enabled: bool,
}

impl ksni::Tray for Ferrite {
    fn id(&self) -> String {
        "co.dealdrive.Ferrite".into()
    }
    fn title(&self) -> String {
        "Ferrite".into()
    }
    fn icon_name(&self) -> String {
        // Grey out the tray glyph when the host isn't running.
        if self.enabled {
            "video-display".into()
        } else {
            "video-display-symbolic".into()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        open_panel();
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        let current_mode = self.mode;
        vec![
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
                    this.host.lock().unwrap().stop();
                    std::process::exit(0);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

impl Ferrite {
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
            self.host.lock().unwrap().start(mode);
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
            self.host.lock().unwrap().start(self.mode);
        } else {
            self.host.lock().unwrap().stop();
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

    let host = Arc::new(Mutex::new(Host::new()));
    if enabled {
        host.lock().unwrap().start(mode);
    }

    let tray = Ferrite {
        host: host.clone(),
        mode,
        enabled,
    };
    // Keep the ksni handle alive for the lifetime of main — it owns the
    // D-Bus registration. The Quit menu calls std::process::exit, so the
    // park loop never returns.
    let _handle = tray.spawn().expect("spawn tray service");
    loop {
        std::thread::park();
    }
}
