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

use std::collections::HashMap;
use std::fs::File;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use ferrite_core::{PointerTool, TouchPoint};
use input_linux::sys::{
    input_event, timeval, ABS_MT_POSITION_X, ABS_MT_POSITION_Y, ABS_MT_SLOT, ABS_MT_TRACKING_ID,
    ABS_PRESSURE, ABS_X, ABS_Y, BTN_TOOL_FINGER, BTN_TOOL_PEN, BTN_TOOL_RUBBER, BTN_TOUCH, EV_ABS,
    EV_KEY, EV_SYN, SYN_REPORT,
};
use input_linux::{
    AbsoluteAxis, AbsoluteInfo, AbsoluteInfoSetup, EventKind, InputId, InputProperty, Key,
    UInputHandle,
};
use tracing::info;

const ABS_MAX: i32 = 0xFFFF;
const PRESSURE_MAX: i32 = 1023;
const MAX_SLOTS: i32 = 10;

pub const TOUCH_NAME_PREFIX: &str = "ferrite virtual touchscreen";
pub const PEN_NAME_PREFIX: &str = "ferrite virtual pen";

struct Dev {
    handle: UInputHandle<File>,
    last_pressed: bool,
    last_tool: Option<i32>, // last asserted BTN_TOOL_*; for the pen device only
}

struct TouchDev {
    handle: UInputHandle<File>,
    /// Maps client-side pointer id (Android `getPointerId`) to a uinput slot
    /// number in `[0, MAX_SLOTS)`. Slot allocation is first-free.
    slots: HashMap<u32, i32>,
    last_any_pressed: bool,
    last_active_slot: i32,
}

#[derive(Clone)]
pub struct InputSink {
    touch: Arc<Mutex<TouchDev>>,
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

    pub fn send_pointer(
        &self,
        x: f32,
        y: f32,
        pressed: bool,
        pressure: f32,
        tool: PointerTool,
        in_range: bool,
    ) {
        // Pen/eraser path. Finger touches now arrive via `send_touches` so we
        // ignore PointerTool::Finger here; legacy callers should not send it.
        if matches!(tool, PointerTool::Finger) {
            return;
        }
        self.send_pen(x, y, pressed, pressure, tool, in_range);
        if self.mirror_pen_to_touch {
            // Mirror the pen position onto the touch device as a single-finger
            // touch so non-tablet apps see cursor movement on the mapped output.
            let single = if pressed {
                vec![TouchPoint { id: u32::MAX, x, y }]
            } else {
                Vec::new()
            };
            self.send_touches(&single);
        }
    }

    /// Snapshot-based MT-B emission. `points` is the full set of currently-down
    /// fingers; anything previously down but absent here is released.
    pub fn send_touches(&self, points: &[TouchPoint]) {
        let mut dev = self.touch.lock().unwrap();
        let mut events: Vec<input_event> = Vec::with_capacity(points.len() * 4 + 6);

        // Release slots whose pointer ids are no longer present.
        let still_down: std::collections::HashSet<u32> = points.iter().map(|p| p.id).collect();
        let to_release: Vec<u32> = dev
            .slots
            .keys()
            .copied()
            .filter(|id| !still_down.contains(id))
            .collect();
        for id in to_release {
            let slot = dev.slots.remove(&id).expect("slot just present");
            if dev.last_active_slot != slot {
                events.push(event(EV_ABS, ABS_MT_SLOT, slot));
                dev.last_active_slot = slot;
            }
            events.push(event(EV_ABS, ABS_MT_TRACKING_ID, -1));
        }

        // Down + move active fingers.
        for p in points {
            let xi = (p.x.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
            let yi = (p.y.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
            let new_finger = !dev.slots.contains_key(&p.id);
            let slot = if new_finger {
                let used: std::collections::HashSet<i32> = dev.slots.values().copied().collect();
                let mut s = 0;
                while used.contains(&s) {
                    s += 1;
                }
                if s >= MAX_SLOTS {
                    continue; // out of slots; drop this finger
                }
                dev.slots.insert(p.id, s);
                s
            } else {
                dev.slots[&p.id]
            };
            if dev.last_active_slot != slot {
                events.push(event(EV_ABS, ABS_MT_SLOT, slot));
                dev.last_active_slot = slot;
            }
            if new_finger {
                events.push(event(EV_ABS, ABS_MT_TRACKING_ID, p.id as i32));
            }
            events.push(event(EV_ABS, ABS_MT_POSITION_X, xi));
            events.push(event(EV_ABS, ABS_MT_POSITION_Y, yi));
        }

        // Single-touch ABS_X/Y emulation for legacy clients (uses first finger).
        if let Some(first) = points.first() {
            let xi = (first.x.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
            let yi = (first.y.clamp(0.0, 1.0) * ABS_MAX as f32) as i32;
            events.push(event(EV_ABS, ABS_X, xi));
            events.push(event(EV_ABS, ABS_Y, yi));
        }

        let any_pressed = !points.is_empty();
        if any_pressed != dev.last_any_pressed {
            let v = if any_pressed { 1 } else { 0 };
            events.push(event(EV_KEY, BTN_TOUCH, v));
            events.push(event(EV_KEY, BTN_TOOL_FINGER, v));
            dev.last_any_pressed = any_pressed;
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

fn create_touch(device_name: &str) -> Result<TouchDev> {
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
    h.set_keybit(Key::ButtonToolFinger)?;
    h.set_absbit(AbsoluteAxis::X)?;
    h.set_absbit(AbsoluteAxis::Y)?;
    h.set_absbit(AbsoluteAxis::MultitouchSlot)?;
    h.set_absbit(AbsoluteAxis::MultitouchTrackingId)?;
    h.set_absbit(AbsoluteAxis::MultitouchPositionX)?;
    h.set_absbit(AbsoluteAxis::MultitouchPositionY)?;
    h.set_propbit(InputProperty::Direct)?;
    let id = InputId {
        bustype: 3,
        vendor: 0xfe71,
        product: 0x17e0,
        version: 1,
    };
    let abs = |axis, min, max| AbsoluteInfoSetup {
        axis,
        info: AbsoluteInfo {
            value: 0,
            minimum: min,
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
        &[
            abs(AbsoluteAxis::X, 0, ABS_MAX),
            abs(AbsoluteAxis::Y, 0, ABS_MAX),
            abs(AbsoluteAxis::MultitouchSlot, 0, MAX_SLOTS - 1),
            // TRACKING_ID is signed: -1 = release, otherwise the client id.
            abs(AbsoluteAxis::MultitouchTrackingId, -1, i32::MAX),
            abs(AbsoluteAxis::MultitouchPositionX, 0, ABS_MAX),
            abs(AbsoluteAxis::MultitouchPositionY, 0, ABS_MAX),
        ],
    )?;
    info!(name, "multi-touch device created at /dev/uinput");
    Ok(TouchDev {
        handle: h,
        slots: HashMap::new(),
        last_any_pressed: false,
        last_active_slot: -1,
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
