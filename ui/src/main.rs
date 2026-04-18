//! Ferrite control panel (throwaway window).
//!
//! Shows connection info + QR, toggles transport (Wi-Fi / adb-reverse USB),
//! and configures cosmic-comp touchscreen output mapping. Does NOT own the
//! `ferrite-host` process — that's managed by `ferrite-tray`. Closing this
//! window only exits the panel; host keeps running.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use cosmic::app::{Core, Settings, Task};
use cosmic::iced::{Alignment, Length, Subscription};
use cosmic::{Application, Element, executor, widget};
use ferrite_core::{Status, status_path};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const TOUCH_DEVICE_NAME: &str = "ferrite virtual touchscreen";

const EVDI_SETUP_CMD: &str = "echo 1 | sudo tee /sys/devices/evdi/add";

fn evdi_present() -> bool {
    std::fs::read_to_string("/sys/devices/evdi/count")
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
        .map(|n| n > 0)
        .unwrap_or(false)
}

fn portal_token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("ferrite").join("portal_token"))
}

fn portal_token_present() -> bool {
    portal_token_path()
        .map(|p| p.exists())
        .unwrap_or(false)
}

fn read_host_status() -> Option<Status> {
    let bytes = std::fs::read(status_path()).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn adb_path() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("HOME") {
        let p = PathBuf::from(home).join("Android/Sdk/platform-tools/adb");
        if p.exists() {
            return Some(p);
        }
    }
    Some(PathBuf::from("adb"))
}

fn adb_devices() -> Vec<String> {
    let Some(adb) = adb_path() else { return Vec::new() };
    let out = match std::process::Command::new(adb).arg("devices").output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .filter_map(|l| {
            let parts: Vec<&str> = l.split_whitespace().collect();
            (parts.len() == 2 && parts[1] == "device").then(|| parts[0].to_string())
        })
        .collect()
}

fn adb_devices_first(devs: &[String]) -> Option<String> {
    devs.first().cloned()
}

fn list_outputs() -> Vec<String> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir("/sys/class/drm") else {
        return v;
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        // expect "card<N>-<connector>", e.g. "card0-HDMI-A-2"
        let after_card = match name.strip_prefix("card") {
            Some(s) => s,
            None => continue,
        };
        let dash = match after_card.find('-') {
            Some(i) => i,
            None => continue,
        };
        let connector = &after_card[dash + 1..];
        if connector.contains("Writeback") {
            continue;
        }
        // include all (connected + disconnected) so evdi shows up too
        v.push(connector.to_string());
    }
    v.sort();
    v.dedup();
    v
}

fn cosmic_input_devices_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(
        base.join("cosmic")
            .join("com.system76.CosmicComp")
            .join("v1")
            .join("input_devices"),
    )
}

#[derive(Serialize, Deserialize, Debug)]
struct OurInputConfig {
    state: DeviceState,
    #[serde(skip_serializing_if = "Option::is_none")]
    map_to_output: Option<String>,
}

#[derive(Serialize, Deserialize, Debug)]
enum DeviceState {
    Enabled,
    #[allow(dead_code)]
    Disabled,
    #[allow(dead_code)]
    DisabledOnExternalMouse,
}

/// Read the current `map_to_output` value for our touchscreen entry, if any.
fn read_current_touch_mapping() -> Option<String> {
    let path = cosmic_input_devices_path()?;
    let s = std::fs::read_to_string(&path).ok()?;
    if s.trim().is_empty() {
        return None;
    }
    let map: std::collections::BTreeMap<String, OurInputConfig> = ron::from_str(&s).ok()?;
    map.get(TOUCH_DEVICE_NAME)?.map_to_output.clone()
}

/// Write our touchscreen entry into the COSMIC input_devices map. Tries to
/// preserve other entries — but their unknown fields (e.g. `acceleration`,
/// `scroll_config`) will be dropped because `OurInputConfig` only knows about
/// the fields we use. For most users `input_devices` is empty, so this is fine.
fn write_touch_mapping(output: Option<&str>) -> Result<(), String> {
    let path = cosmic_input_devices_path().ok_or_else(|| "no XDG_CONFIG_HOME".to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }

    let mut map: std::collections::BTreeMap<String, OurInputConfig> =
        match std::fs::read_to_string(&path) {
            Ok(s) if !s.trim().is_empty() => ron::from_str(&s).unwrap_or_default(),
            _ => std::collections::BTreeMap::new(),
        };

    map.insert(
        TOUCH_DEVICE_NAME.to_string(),
        OurInputConfig {
            state: DeviceState::Enabled,
            map_to_output: output.map(|s| s.to_string()),
        },
    );

    let pretty = ron::ser::to_string_pretty(&map, ron::ser::PrettyConfig::default())
        .map_err(|e| e.to_string())?;
    std::fs::write(&path, pretty).map_err(|e| e.to_string())?;
    info!(path = %path.display(), ?output, "touch mapping written");
    Ok(())
}

fn adb_reverse(serial: Option<&str>, port: u16) -> Result<(), String> {
    let adb = adb_path().ok_or_else(|| "adb not found".to_string())?;
    let mut cmd = std::process::Command::new(adb);
    if let Some(s) = serial {
        cmd.arg("-s").arg(s);
    }
    cmd.args([
        "reverse",
        &format!("tcp:{}", port),
        &format!("tcp:{}", port),
    ]);
    let out = cmd.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

const PORT: u16 = 7543;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Transport {
    Wifi,
    Usb,
}
const TRANSPORT_OPTIONS: [Transport; 2] = [Transport::Wifi, Transport::Usb];
static TRANSPORT_LABELS: [&str; 2] = ["Wi-Fi (LAN)", "USB (adb reverse)"];

#[derive(Clone, Debug)]
enum Message {
    TransportSelected(usize),
    TouchOutputSelected(usize),
    CopyEvdiCmd,
    ForgetPortalGrant,
    Tick,
}

struct App {
    core: Core,
    local_ip: Option<IpAddr>,
    qr_png: Option<Vec<u8>>,
    evdi_present: bool,
    portal_token_present: bool,
    host_status: Option<Status>,
    transport: Transport,
    adb_devices: Vec<String>,
    adb_reverse_ok: Option<Result<(), String>>,
    outputs: Vec<String>,
    touch_labels: Vec<String>,
    touch_output: Option<String>,
    touch_apply_err: Option<String>,
}

impl Application for App {
    type Executor = executor::Default;
    type Flags = ();
    type Message = Message;
    const APP_ID: &'static str = "co.dealdrive.Ferrite";

    fn core(&self) -> &Core {
        &self.core
    }
    fn core_mut(&mut self) -> &mut Core {
        &mut self.core
    }

    fn init(core: Core, _flags: ()) -> (Self, Task<Self::Message>) {
        let local_ip = local_ip_address::local_ip().ok();
        let qr_png = qr_png_for_url(&connect_url(local_ip, PORT, Transport::Wifi));
        let app = App {
            core,
            local_ip,
            qr_png,
            evdi_present: evdi_present(),
            portal_token_present: portal_token_present(),
            host_status: None,
            transport: Transport::Wifi,
            adb_devices: Vec::new(),
            adb_reverse_ok: None,
            outputs: Vec::new(),
            touch_labels: Vec::new(),
            touch_output: read_current_touch_mapping(),
            touch_apply_err: None,
        };
        (app, Task::none())
    }

    fn subscription(&self) -> Subscription<Self::Message> {
        cosmic::iced::time::every(Duration::from_millis(250)).map(|_| Message::Tick)
    }


    fn view(&self) -> Element<'_, Self::Message> {
        let spacing = cosmic::theme::spacing();

        let header = widget::text::title2("Ferrite");
        let status_text = match self.host_status.as_ref() {
            Some(s) => format!("{} — {} client(s)", s.mode, s.clients.len()),
            None => "host not running".into(),
        };
        let status_line = widget::row::with_children(vec![
            widget::text::body("Status:").into(),
            widget::text::body(status_text).into(),
        ])
        .spacing(spacing.space_xs);

        let ip_str = self
            .local_ip
            .map(|ip| ip.to_string())
            .unwrap_or_else(|| "(unknown)".into());
        let conn_block = widget::column::with_children(vec![
            widget::text::heading("Connect from Android").into(),
            widget::text::body(format!("Host: {}", ip_str)).into(),
            widget::text::body(format!("Port: {}", PORT)).into(),
        ])
        .spacing(spacing.space_xxs);

        let qr_centered: Element<'_, Self::Message> = match &self.qr_png {
            Some(bytes) => widget::container(
                widget::image(widget::image::Handle::from_bytes(bytes.clone()))
                    .width(Length::Fixed(240.0))
                    .height(Length::Fixed(240.0)),
            )
            .width(Length::Fill)
            .align_x(Alignment::Center)
            .into(),
            None => widget::text::caption("(no QR — add a local IP)").into(),
        };

        let transport_idx = TRANSPORT_OPTIONS.iter().position(|t| *t == self.transport);
        let transport_row = widget::row::with_children(vec![
            widget::text::body("Transport:").into(),
            widget::dropdown(&TRANSPORT_LABELS, transport_idx, Message::TransportSelected)
                .width(Length::Fill)
                .into(),
        ])
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center);

        let touch_idx = match self.touch_output.as_deref() {
            None => Some(0),
            Some(o) => self.outputs.iter().position(|n| n == o).map(|i| i + 1),
        };
        let touch_row = widget::row::with_children(vec![
            widget::text::body("Touch maps to:").into(),
            widget::dropdown(&self.touch_labels, touch_idx, Message::TouchOutputSelected)
                .width(Length::Fill)
                .into(),
        ])
        .spacing(spacing.space_xs)
        .align_y(Alignment::Center);
        let touch_caption: Element<'_, Self::Message> = match &self.touch_apply_err {
            Some(e) => widget::text::caption(format!("err: {e}")).into(),
            None => widget::text::caption(
                "Edits ~/.config/cosmic/.../input_devices. May need re-login to apply.",
            )
            .into(),
        };

        let usb_status: Option<Element<'_, Self::Message>> =
            if self.transport == Transport::Usb {
                let txt = match (&self.adb_reverse_ok, self.adb_devices.first()) {
                    (Some(Ok(())), Some(serial)) => format!("USB device {serial} forwarded ✓"),
                    (Some(Err(e)), _) => format!("adb: {e}"),
                    _ => "Looking for USB device...".to_string(),
                };
                Some(widget::text::caption(txt).into())
            } else {
                None
            };

        let portal_row: Option<Element<'_, Self::Message>> =
            if self.portal_token_present {
                Some(
                    widget::row::with_children(vec![
                        widget::text::caption("Portal grant cached — won't ask again").into(),
                        widget::Space::new().width(Length::Fill).into(),
                        widget::button::standard("Forget")
                            .on_press(Message::ForgetPortalGrant)
                            .into(),
                    ])
                    .align_y(Alignment::Center)
                    .spacing(spacing.space_s)
                    .into(),
                )
            } else {
                None
            };

        let evdi_banner: Option<Element<'_, Self::Message>> =
            if !self.evdi_present {
                let text_col = widget::column::with_children(vec![
                    widget::text::heading("Virtual monitor needs setup").into(),
                    widget::text::body(
                        "No evdi device exists yet. Run this once (per boot):",
                    )
                    .into(),
                    widget::text::monotext(EVDI_SETUP_CMD).into(),
                ])
                .spacing(spacing.space_xxs);
                let copy_btn = widget::button::standard("Copy").on_press(Message::CopyEvdiCmd);
                Some(
                    widget::container(
                        widget::row::with_children(vec![
                            text_col.into(),
                            widget::Space::new().width(Length::Fill).into(),
                            copy_btn.into(),
                        ])
                        .align_y(Alignment::Center)
                        .spacing(spacing.space_s),
                    )
                    .padding(spacing.space_s)
                    .class(cosmic::theme::Container::Card)
                    .into(),
                )
            } else {
                None
            };

        let clients_section: Option<Element<'_, Self::Message>> =
            self.host_status.as_ref().map(|s| {
                let header = widget::text::heading(format!(
                    "Clients ({})",
                    s.clients.len()
                ));
                if s.clients.is_empty() {
                    widget::column::with_children(vec![
                        header.into(),
                        widget::text::caption("(none connected)").into(),
                    ])
                    .spacing(spacing.space_xxs)
                    .into()
                } else {
                    let mut col = widget::column::with_capacity(s.clients.len() + 1)
                        .spacing(spacing.space_xxs);
                    col = col.push(header);
                    for c in &s.clients {
                        col = col.push(widget::text::body(format!(
                            "• {}  ({}×{})",
                            c.peer, c.width, c.height
                        )));
                    }
                    col.into()
                }
            });

        let mut children: Vec<Element<'_, Self::Message>> = vec![
            header.into(),
            status_line.into(),
            widget::divider::horizontal::default().into(),
            conn_block.into(),
            qr_centered,
            transport_row.into(),
        ];
        if let Some(s) = usb_status {
            children.push(s);
        }
        if let Some(b) = evdi_banner {
            children.push(widget::divider::horizontal::default().into());
            children.push(b);
        }
        if let Some(b) = portal_row {
            children.push(b);
        }
        children.push(widget::divider::horizontal::default().into());
        children.push(touch_row.into());
        children.push(touch_caption);
        if let Some(c) = clients_section {
            children.push(widget::divider::horizontal::default().into());
            children.push(c);
        }
        let body = widget::column::with_children(children)
            .spacing(spacing.space_m)
            .max_width(520);

        widget::container(body)
            .width(Length::Fill)
            .height(Length::Fill)
            .align_x(Alignment::Center)
            .align_y(Alignment::Start)
            .padding(spacing.space_l)
            .into()
    }

    fn update(&mut self, message: Self::Message) -> Task<Self::Message> {
        match message {
            Message::TransportSelected(idx) => {
                if let Some(t) = TRANSPORT_OPTIONS.get(idx).copied() {
                    if t != self.transport {
                        self.transport = t;
                        self.qr_png =
                            qr_png_for_url(&connect_url(self.local_ip, PORT, t));
                        if t == Transport::Wifi {
                            // Best-effort cleanup of previous reverse mapping.
                            if let Some(adb) = adb_path() {
                                let _ = std::process::Command::new(adb)
                                    .args([
                                        "reverse",
                                        "--remove",
                                        &format!("tcp:{}", PORT),
                                    ])
                                    .status();
                            }
                            self.adb_reverse_ok = None;
                        }
                    }
                }
                Task::none()
            }
            Message::TouchOutputSelected(idx) => {
                // idx 0 == "Auto (no override)"; 1.. == self.outputs[idx-1]
                let target = if idx == 0 {
                    None
                } else {
                    self.outputs.get(idx - 1).cloned()
                };
                match write_touch_mapping(target.as_deref()) {
                    Ok(()) => {
                        self.touch_output = target;
                        self.touch_apply_err = None;
                    }
                    Err(e) => {
                        warn!("write touch mapping: {e}");
                        self.touch_apply_err = Some(e);
                    }
                }
                Task::none()
            }
            Message::Tick => {
                self.evdi_present = evdi_present();
                self.portal_token_present = portal_token_present();
                self.host_status = read_host_status();
                self.outputs = list_outputs();
                self.touch_labels = std::iter::once("Auto (no override)".to_string())
                    .chain(self.outputs.iter().cloned())
                    .collect();
                if self.transport == Transport::Usb {
                    self.adb_devices = adb_devices();
                    self.adb_reverse_ok = Some(
                        adb_devices_first(&self.adb_devices)
                            .ok_or_else(|| "no USB device".to_string())
                            .and_then(|s| {
                                adb_reverse(Some(&s), PORT).map_err(|e| e.to_string())
                            }),
                    );
                } else {
                    self.adb_reverse_ok = None;
                }
                Task::none()
            }
            Message::CopyEvdiCmd => {
                cosmic::iced::clipboard::write(EVDI_SETUP_CMD.to_string())
            }
            Message::ForgetPortalGrant => {
                if let Some(p) = portal_token_path() {
                    if let Err(e) = std::fs::remove_file(&p) {
                        warn!(path = %p.display(), error = %e, "could not delete token");
                    } else {
                        info!(path = %p.display(), "portal grant forgotten");
                    }
                    self.portal_token_present = portal_token_present();
                }
                Task::none()
            }
        }
    }
}

fn connect_url(ip: Option<IpAddr>, port: u16, transport: Transport) -> String {
    match transport {
        Transport::Usb => format!("ferrite://127.0.0.1:{}", port),
        Transport::Wifi => match ip {
            Some(ip) => format!("ferrite://{}:{}", ip, port),
            None => format!("ferrite://127.0.0.1:{}", port),
        },
    }
}

/// Render the QR code as a PNG (luma8). Returns the encoded bytes ready to
/// hand to `widget::image::Handle::from_bytes`.
fn qr_png_for_url(text: &str) -> Option<Vec<u8>> {
    use image::{ImageFormat, Luma};
    use qrcode::{EcLevel, QrCode};

    let code = QrCode::with_error_correction_level(text, EcLevel::L)
        .map_err(|e| warn!("qr generate failed: {e}"))
        .ok()?;
    let img = code
        .render::<Luma<u8>>()
        .module_dimensions(8, 8)
        .quiet_zone(true)
        .build();
    let mut png = Vec::new();
    image::DynamicImage::ImageLuma8(img)
        .write_to(&mut std::io::Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| warn!("qr encode failed: {e}"))
        .ok()?;
    Some(png)
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    cosmic::app::run::<App>(Settings::default(), ())?;
    Ok(())
}
