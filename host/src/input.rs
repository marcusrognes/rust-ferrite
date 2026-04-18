//! Two virtual input devices via `/dev/uinput`:
//!
//! - **`ferrite virtual touchscreen`**: classic touchscreen — `ABS_X/Y`,
//!   `BTN_TOUCH`, `BTN_TOOL_FINGER`, `INPUT_PROP_DIRECT`. Used for finger input.
//! - **`ferrite virtual pen`**: pen tablet — `ABS_X/Y`, `ABS_PRESSURE`,
//!   `BTN_TOUCH`, `BTN_TOOL_PEN`, `BTN_TOOL_RUBBER`, `BTN_STYLUS`,
//!   `INPUT_PROP_DIRECT`. Used for stylus input (S-Pen, Apple Pencil, etc.).
//!
//! Splitting them keeps libinput's classification clean — a single device with
//! both finger and pen tool buttons isn't a combination libinput's wired up
//! to handle gracefully on most compositors. Apps that speak Wayland's
//! `tablet_v2` protocol will see the pen device as a tablet and receive
//! pressure-sensitive events.
//!
//! Each device can be bound to a different output via the
//! `~/.config/cosmic/com.system76.CosmicComp/v1/input_devices` config (the UI
//! exposes the touchscreen one).

use std::fs::File;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ferrite_core::PointerTool;
use input_linux::sys::{
    input_event, timeval, ABS_PRESSURE, ABS_X, ABS_Y, BTN_TOOL_FINGER, BTN_TOOL_PEN,
    BTN_TOOL_RUBBER, BTN_TOUCH, EV_ABS, EV_KEY, EV_SYN, SYN_REPORT,
};
use input_linux::{
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, InputId, InputProperty, Key,
    UInputHandle,
};
use tracing::info;

const ABS_MAX: i32 = 0xFFFF;
const PRESSURE_MAX: i32 = 1023;

pub const TOUCH_NAME_PREFIX: &str = "ferrite virtual touchscreen";
pub const PEN_NAME_PREFIX: &str = "ferrite virtual pen";

struct Dev {
    handle: UInputHandle<File>,
    last_pressed: bool,
    last_tool: Option<i32>, // last asserted BTN_TOOL_*; for the pen device only
}

#[derive(Clone)]
pub struct InputSink {
    touch: Arc<Mutex<Dev>>,
    pen: Arc<Mutex<Dev>>,
    mirror_pen_to_touch: bool,
}

impl InputSink {
    pub fn new(device_name: &str) -> Result<Self> {
        let touch = create_touch(device_name).context("create touchscreen device")?;
        let pen = create_pen(device_name).context("create pen device")?;
        // FERRITE_PEN_MIRROR=0 disables touchscreen mirror (use when the
        // target app speaks tablet_v2 and gets confused by parallel touch
        // events — e.g. Krita with touch-input enabled).
        let mirror_pen_to_touch = std::env::var("FERRITE_PEN_MIRROR")
            .map(|v| v != "0")
            .unwrap_or(true);
        info!(mirror_pen_to_touch, "input sink ready");
        Ok(Self {
            touch: Arc::new(Mutex::new(touch)),
            pen: Arc::new(Mutex::new(pen)),
            mirror_pen_to_touch,
        })
    }

    pub fn send(
        &self,
        x: f32,
        y: f32,
        pressed: bool,
        pressure: f32,
        tool: PointerTool,
        in_range: bool,
    ) {
        match tool {
            PointerTool::Finger => self.send_finger(x, y, pressed),
            PointerTool::Pen | PointerTool::Eraser => {
                let was_pressed = self.pen.lock().unwrap().last_pressed;
                self.send_pen(x, y, pressed, pressure, tool, in_range);
                if self.mirror_pen_to_touch && (pressed || was_pressed) {
                    self.send_finger(x, y, pressed);
                }
            }
        }
    }

    fn send_finger(&self, x: f32, y: f32, pressed: bool) {
        let xi = (x.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
        let yi = (y.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
        let mut dev = self.touch.lock().unwrap();
        let mut events = vec![event(EV_ABS, ABS_X, xi), event(EV_ABS, ABS_Y, yi)];
        if pressed != dev.last_pressed {
            let v = if pressed { 1 } else { 0 };
            events.push(event(EV_KEY, BTN_TOOL_FINGER, v));
            events.push(event(EV_KEY, BTN_TOUCH, v));
            dev.last_pressed = pressed;
        }
        events.push(event(EV_SYN, SYN_REPORT, 0));
        let _ = dev.handle.write(&events);
    }

    fn send_pen(
        &self,
        x: f32,
        y: f32,
        pressed: bool,
        pressure: f32,
        tool: PointerTool,
        in_range: bool,
    ) {
        let xi = (x.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
        let yi = (y.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
        let pi = (pressure.clamp(0.0, 1.0) * PRESSURE_MAX as f32) as i32;
        let tool_btn = if matches!(tool, PointerTool::Eraser) {
            BTN_TOOL_RUBBER
        } else {
            BTN_TOOL_PEN
        };

        let mut dev = self.pen.lock().unwrap();

        // Proximity-in must land in its own SYN frame before touch/motion, or
        // libinput drops the event as out-of-spec.
        if in_range && dev.last_tool != Some(tool_btn) {
            let mut prox = Vec::with_capacity(3);
            if let Some(prev) = dev.last_tool {
                prox.push(event(EV_KEY, prev, 0));
            }
            prox.push(event(EV_KEY, tool_btn, 1));
            prox.push(event(EV_SYN, SYN_REPORT, 0));
            let _ = dev.handle.write(&prox);
            dev.last_tool = Some(tool_btn);
        }

        if in_range {
            let mut events = vec![
                event(EV_ABS, ABS_X, xi),
                event(EV_ABS, ABS_Y, yi),
                event(EV_ABS, ABS_PRESSURE, pi),
            ];
            if pressed != dev.last_pressed {
                events.push(event(EV_KEY, BTN_TOUCH, if pressed { 1 } else { 0 }));
                dev.last_pressed = pressed;
            }
            events.push(event(EV_SYN, SYN_REPORT, 0));
            let _ = dev.handle.write(&events);
        }

        // Tool out-of-proximity, in its own frame. Drop touch first if still held.
        if !in_range {
            let mut prox = Vec::new();
            if dev.last_pressed {
                prox.push(event(EV_KEY, BTN_TOUCH, 0));
                dev.last_pressed = false;
            }
            if let Some(t) = dev.last_tool.take() {
                prox.push(event(EV_KEY, t, 0));
            }
            if !prox.is_empty() {
                prox.push(event(EV_SYN, SYN_REPORT, 0));
                let _ = dev.handle.write(&prox);
            }
        }
    }
}

fn create_touch(device_name: &str) -> Result<Dev> {
    let file = File::options()
        .write(true)
        .read(true)
        .open("/dev/uinput")
        .context("open /dev/uinput")?;
    let h = UInputHandle::new(file);
    h.set_evbit(EventKind::Key)?;
    h.set_evbit(EventKind::Absolute)?;
    h.set_evbit(EventKind::Synchronize)?;
    // No ButtonLeft — its presence makes libinput classify the device as a
    // pointer/mouse instead of a touchscreen, which then bypasses the
    // touch-only `map_to_output` config in cosmic-comp.
    h.set_keybit(Key::ButtonTouch)?;
    h.set_keybit(Key::ButtonToolFinger)?;
    h.set_absbit(AbsoluteAxis::X)?;
    h.set_absbit(AbsoluteAxis::Y)?;
    h.set_propbit(InputProperty::Direct)?;
    let id = InputId {
        bustype: 3,
        vendor: 0xfe71,
        product: 0x17e0,
        version: 1,
    };
    let abs = |axis, max| AbsoluteInfoSetup {
        axis,
        info: AbsoluteInfo {
            value: 0,
            minimum: 0,
            maximum: max,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        },
    };
    let name = format!("{TOUCH_NAME_PREFIX} ({device_name})");
    h.create(
        &id,
        name.as_bytes(),
        0,
        &[abs(AbsoluteAxis::X, ABS_MAX), abs(AbsoluteAxis::Y, ABS_MAX)],
    )?;
    info!(name, "touchscreen device created at /dev/uinput");
    Ok(Dev {
        handle: h,
        last_pressed: false,
        last_tool: None,
    })
}

fn create_pen(device_name: &str) -> Result<Dev> {
    let file = File::options()
        .write(true)
        .read(true)
        .open("/dev/uinput")
        .context("open /dev/uinput")?;
    let h = UInputHandle::new(file);
    h.set_evbit(EventKind::Key)?;
    h.set_evbit(EventKind::Absolute)?;
    h.set_evbit(EventKind::Synchronize)?;
    h.set_keybit(Key::ButtonTouch)?;
    h.set_keybit(Key::ButtonToolPen)?;
    h.set_keybit(Key::ButtonToolRubber)?;
    h.set_keybit(Key::ButtonStylus)?;
    h.set_absbit(AbsoluteAxis::X)?;
    h.set_absbit(AbsoluteAxis::Y)?;
    h.set_absbit(AbsoluteAxis::Pressure)?;
    h.set_propbit(InputProperty::Direct)?;
    let id = InputId {
        bustype: 3,
        vendor: 0xfe72,
        product: 0x17e1,
        version: 1,
    };
    // libinput refuses to classify a device as a tablet unless ABS_X/Y carry
    // a non-zero resolution (units-per-mm). Pick something plausible for a
    // ~250mm-wide tablet — exact value doesn't matter for absolute mapping
    // since the compositor scales to the assigned output anyway.
    let abs_res = |axis, max, res| AbsoluteInfoSetup {
        axis,
        info: AbsoluteInfo {
            value: 0,
            minimum: 0,
            maximum: max,
            fuzz: 0,
            flat: 0,
            resolution: res,
        },
    };
    let name = format!("{PEN_NAME_PREFIX} ({device_name})");
    h.create(
        &id,
        name.as_bytes(),
        0,
        &[
            abs_res(AbsoluteAxis::X, ABS_MAX, 200),
            abs_res(AbsoluteAxis::Y, ABS_MAX, 200),
            abs_res(AbsoluteAxis::Pressure, PRESSURE_MAX, 1),
        ],
    )?;
    info!(name, "pen tablet device created at /dev/uinput");
    Ok(Dev {
        handle: h,
        last_pressed: false,
        last_tool: None,
    })
}

fn event(type_: i32, code: i32, value: i32) -> input_event {
    input_event {
        time: timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: type_ as u16,
        code: code as u16,
        value,
    }
}
