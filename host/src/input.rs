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
    input_event, timeval, ABS_PRESSURE, ABS_X, ABS_Y, BTN_LEFT, BTN_TOOL_FINGER, BTN_TOOL_PEN,
    BTN_TOOL_RUBBER, BTN_TOUCH, EV_ABS, EV_KEY, EV_SYN, SYN_REPORT,
};
use input_linux::{
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, InputId, InputProperty, Key,
    UInputHandle,
};
use tracing::info;

const ABS_MAX: i32 = 0xFFFF;
const PRESSURE_MAX: i32 = 1023;

pub const TOUCH_NAME: &str = "ferrite virtual touchscreen";
pub const PEN_NAME: &str = "ferrite virtual pen";

struct Dev {
    handle: UInputHandle<File>,
    last_pressed: bool,
    last_tool: Option<i32>, // last asserted BTN_TOOL_*; for the pen device only
}

#[derive(Clone)]
pub struct InputSink {
    touch: Arc<Mutex<Dev>>,
    pen: Arc<Mutex<Dev>>,
}

impl InputSink {
    pub fn new() -> Result<Self> {
        let touch = create_touch().context("create touchscreen device")?;
        let pen = create_pen().context("create pen device")?;
        Ok(Self {
            touch: Arc::new(Mutex::new(touch)),
            pen: Arc::new(Mutex::new(pen)),
        })
    }

    pub fn send(&self, x: f32, y: f32, pressed: bool, pressure: f32, tool: PointerTool) {
        match tool {
            PointerTool::Finger => self.send_finger(x, y, pressed),
            PointerTool::Pen | PointerTool::Eraser => {
                self.send_pen(x, y, pressed, pressure, tool)
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
            events.push(event(EV_KEY, BTN_LEFT, v));
            dev.last_pressed = pressed;
        }
        events.push(event(EV_SYN, SYN_REPORT, 0));
        let _ = dev.handle.write(&events);
    }

    fn send_pen(&self, x: f32, y: f32, pressed: bool, pressure: f32, tool: PointerTool) {
        let xi = (x.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
        let yi = (y.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
        let pi = (pressure.clamp(0.0, 1.0) * PRESSURE_MAX as f32) as i32;
        let tool_btn = if matches!(tool, PointerTool::Eraser) {
            BTN_TOOL_RUBBER
        } else {
            BTN_TOOL_PEN
        };

        let mut dev = self.pen.lock().unwrap();
        let mut events = vec![
            event(EV_ABS, ABS_X, xi),
            event(EV_ABS, ABS_Y, yi),
            event(EV_ABS, ABS_PRESSURE, pi),
        ];

        // BTN_TOOL_<x> latches "in proximity"; BTN_TOUCH latches contact.
        if pressed && !dev.last_pressed {
            // Drop any previously-asserted tool that isn't the active one.
            if let Some(prev) = dev.last_tool {
                if prev != tool_btn {
                    events.push(event(EV_KEY, prev, 0));
                }
            }
            events.push(event(EV_KEY, tool_btn, 1));
            events.push(event(EV_KEY, BTN_TOUCH, 1));
            events.push(event(EV_KEY, BTN_LEFT, 1));
            dev.last_tool = Some(tool_btn);
        } else if !pressed && dev.last_pressed {
            events.push(event(EV_KEY, BTN_TOUCH, 0));
            events.push(event(EV_KEY, BTN_LEFT, 0));
            if let Some(t) = dev.last_tool.take() {
                events.push(event(EV_KEY, t, 0));
            }
        }
        dev.last_pressed = pressed;

        events.push(event(EV_SYN, SYN_REPORT, 0));
        let _ = dev.handle.write(&events);
    }
}

fn create_touch() -> Result<Dev> {
    let file = File::options()
        .write(true)
        .read(true)
        .open("/dev/uinput")
        .context("open /dev/uinput")?;
    let h = UInputHandle::new(file);
    h.set_evbit(EventKind::Key)?;
    h.set_evbit(EventKind::Absolute)?;
    h.set_evbit(EventKind::Synchronize)?;
    h.set_keybit(Key::ButtonLeft)?;
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
    h.create(
        &id,
        TOUCH_NAME.as_bytes(),
        0,
        &[abs(AbsoluteAxis::X, ABS_MAX), abs(AbsoluteAxis::Y, ABS_MAX)],
    )?;
    info!("touchscreen device created at /dev/uinput");
    Ok(Dev {
        handle: h,
        last_pressed: false,
        last_tool: None,
    })
}

fn create_pen() -> Result<Dev> {
    let file = File::options()
        .write(true)
        .read(true)
        .open("/dev/uinput")
        .context("open /dev/uinput")?;
    let h = UInputHandle::new(file);
    h.set_evbit(EventKind::Key)?;
    h.set_evbit(EventKind::Absolute)?;
    h.set_evbit(EventKind::Synchronize)?;
    h.set_keybit(Key::ButtonLeft)?;
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
    h.create(
        &id,
        PEN_NAME.as_bytes(),
        0,
        &[
            abs(AbsoluteAxis::X, ABS_MAX),
            abs(AbsoluteAxis::Y, ABS_MAX),
            abs(AbsoluteAxis::Pressure, PRESSURE_MAX),
        ],
    )?;
    info!("pen tablet device created at /dev/uinput");
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
