//! Virtual monitor via `evdi` kernel module.
//!
//! `start()` creates a new evdi device, connects it with a fake EDID, and spawns
//! an OS thread that runs the evdi event loop. The compositor (COSMIC) sees the
//! device as a real monitor and the user can place it in Display settings.
//! Framebuffer updates from the compositor are converted to RGB and published
//! on the existing `FrameTx` watch channel.

use std::ffi::c_void;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};
use tracing::{error, info, warn};

use crate::capture::{Frame, FrameTx};

// --- FFI to libevdi (evdi_lib.h) ---

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct EvdiRect {
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
}

#[repr(C)]
#[derive(Default, Clone, Copy, Debug)]
struct EvdiMode {
    width: i32,
    height: i32,
    refresh_rate: i32,
    bits_per_pixel: i32,
    pixel_format: u32,
}

#[repr(C)]
struct EvdiBuffer {
    id: i32,
    buffer: *mut c_void,
    width: i32,
    height: i32,
    stride: i32,
    rects: *mut EvdiRect,
    rect_count: i32,
}

#[repr(C)]
struct EvdiCursorSet {
    hot_x: i32,
    hot_y: i32,
    width: u32,
    height: u32,
    enabled: u8,
    buffer_length: u32,
    buffer: *mut u32,
    pixel_format: u32,
    stride: u32,
}

#[repr(C)]
struct EvdiCursorMove {
    x: i32,
    y: i32,
}

#[repr(C)]
struct EvdiDdcciData {
    address: u16,
    flags: u16,
    buffer_length: u32,
    buffer: *mut u8,
}

#[repr(C)]
struct EvdiEventContext {
    dpms: Option<extern "C" fn(i32, *mut c_void)>,
    mode_changed: Option<extern "C" fn(EvdiMode, *mut c_void)>,
    update_ready: Option<extern "C" fn(i32, *mut c_void)>,
    crtc_state: Option<extern "C" fn(i32, *mut c_void)>,
    cursor_set: Option<extern "C" fn(EvdiCursorSet, *mut c_void)>,
    cursor_move: Option<extern "C" fn(EvdiCursorMove, *mut c_void)>,
    ddcci: Option<extern "C" fn(EvdiDdcciData, *mut c_void)>,
    user_data: *mut c_void,
}

// Matches `enum evdi_device_status` in evdi_lib.h.
const EVDI_AVAILABLE: i32 = 0;
#[allow(dead_code)]
const EVDI_UNRECOGNIZED: i32 = 1;
#[allow(dead_code)]
const EVDI_NOT_PRESENT: i32 = 2;

#[link(name = "evdi")]
unsafe extern "C" {
    fn evdi_check_device(device: i32) -> i32;
    fn evdi_add_device() -> i32;
    fn evdi_open(device: i32) -> *mut c_void;
    fn evdi_close(handle: *mut c_void);
    fn evdi_connect(
        handle: *mut c_void,
        edid: *const u8,
        edid_length: u32,
        sku_area_limit: u32,
    );
    fn evdi_disconnect(handle: *mut c_void);
    fn evdi_register_buffer(handle: *mut c_void, buffer: EvdiBuffer);
    fn evdi_unregister_buffer(handle: *mut c_void, buffer_id: i32);
    fn evdi_request_update(handle: *mut c_void, buffer_id: i32) -> bool;
    fn evdi_grab_pixels(handle: *mut c_void, rects: *mut EvdiRect, num_rects: *mut i32);
    fn evdi_handle_events(handle: *mut c_void, ctx: *mut EvdiEventContext);
    fn evdi_get_event_ready(handle: *mut c_void) -> i32;
}

// First 124 bytes of a 1920x1080 @ 60Hz EDID (Linux FHD layout). Missing trailing
// monitor-name padding (2 bytes), extension-count (1 byte), and checksum (1
// byte) — filled in at runtime in `edid_1080p()`.
#[rustfmt::skip]
const EDID_BASE: [u8; 124] = [
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00,
    0x31, 0xd8, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x05, 0x16, 0x01, 0x03, 0x6d, 0x32, 0x1c, 0x78,
    0xea, 0x5e, 0xc0, 0xa4, 0x59, 0x4a, 0x98, 0x25,
    0x20, 0x50, 0x54, 0x00, 0x00, 0x00, 0xd1, 0xc0,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x3a,
    0x80, 0x18, 0x71, 0x38, 0x2d, 0x40, 0x58, 0x2c,
    0x45, 0x00, 0xf4, 0x19, 0x11, 0x00, 0x00, 0x1e,
    0x00, 0x00, 0x00, 0xff, 0x00, 0x4c, 0x69, 0x6e,
    0x75, 0x78, 0x20, 0x23, 0x30, 0x0a, 0x20, 0x20,
    0x20, 0x00, 0x00, 0x00, 0xfd, 0x00, 0x3b, 0x3d,
    0x42, 0x42, 0x1e, 0x0a, 0x20, 0x20, 0x20, 0x20,
    0x20, 0x20, 0x00, 0x00, 0x00, 0xfc, 0x00, 0x4c,
    0x69, 0x6e, 0x75, 0x78, 0x20, 0x46, 0x48, 0x44,
    0x0a, 0x20, 0x20, 0x20,
];

fn edid_1080p() -> [u8; 128] {
    let mut out = [0u8; 128];
    out[..124].copy_from_slice(&EDID_BASE);
    out[124] = 0x20; // finish monitor-name padding ("   ")
    out[125] = 0x20;
    out[126] = 0; // no extension blocks
    let sum: u32 = out[..127].iter().map(|&b| b as u32).sum();
    out[127] = (256u32.wrapping_sub(sum & 0xff) & 0xff) as u8;
    out
}

// --- State shared between event callbacks (single-threaded event loop) ---

struct State {
    tx: FrameTx,
    handle: *mut c_void,
    width: i32,
    height: i32,
    stride: i32,
    buffer_id: i32,
    bgra: Vec<u8>, // destination for evdi_grab_pixels (XRGB/BGRA — 4 bytes/pixel)
    rgb: Vec<u8>,  // tightly packed RGB published on watch
    registered: bool,
    pending_update: bool,
}
// Safety: only accessed from the single evdi thread; callbacks reenter via raw
// ptr but are invoked from that same thread during evdi_handle_events.
unsafe impl Send for State {}

extern "C" fn on_mode_changed(mode: EvdiMode, user_data: *mut c_void) {
    let state = unsafe { &mut *(user_data as *mut State) };
    info!(
        w = mode.width,
        h = mode.height,
        fps = mode.refresh_rate,
        bpp = mode.bits_per_pixel,
        "evdi mode changed"
    );
    if mode.width <= 0 || mode.height <= 0 {
        return;
    }
    if state.registered {
        unsafe { evdi_unregister_buffer(state.handle, state.buffer_id) };
        state.registered = false;
    }
    state.width = mode.width;
    state.height = mode.height;
    state.stride = mode.width * 4;
    state.bgra = vec![0u8; (mode.width * mode.height * 4) as usize];
    state.rgb = vec![0u8; (mode.width * mode.height * 3) as usize];

    // rects array we hand to grab_pixels; evdi writes dirty-rect list here.
    // We keep a static-ish array per-State to avoid alloc per frame — but the
    // pointer must stay valid across register + grab, so store on State too.
    // Simpler: grab_pixels is called only from our update_ready; we pass a
    // local stack array there, not stored here. register_buffer ignores rects.
    let mut dummy_rect = EvdiRect {
        x1: 0,
        y1: 0,
        x2: 0,
        y2: 0,
    };
    let evdi_buf = EvdiBuffer {
        id: state.buffer_id,
        buffer: state.bgra.as_mut_ptr() as *mut c_void,
        width: mode.width,
        height: mode.height,
        stride: state.stride,
        rects: &mut dummy_rect as *mut EvdiRect,
        rect_count: 0,
    };
    unsafe { evdi_register_buffer(state.handle, evdi_buf) };
    state.registered = true;
    info!(
        id = state.buffer_id,
        size = state.bgra.len(),
        "evdi buffer registered"
    );
}

extern "C" fn on_update_ready(buffer_id: i32, user_data: *mut c_void) {
    let state = unsafe { &mut *(user_data as *mut State) };
    if buffer_id != state.buffer_id || !state.registered {
        return;
    }

    let mut rects = [EvdiRect::default(); 16];
    let mut num: i32 = rects.len() as i32;
    unsafe { evdi_grab_pixels(state.handle, rects.as_mut_ptr(), &mut num) };

    bgra_to_rgb(
        &state.bgra,
        state.width as usize,
        state.height as usize,
        state.stride as usize,
        &mut state.rgb,
    );
    let _ = state.tx.send(Some(Arc::new(Frame {
        width: state.width as u32,
        height: state.height as u32,
        rgb: state.rgb.clone(),
    })));
    state.pending_update = false;
}

extern "C" fn on_dpms(mode: i32, _user_data: *mut c_void) {
    info!(mode, "evdi dpms");
}

extern "C" fn on_crtc_state(state: i32, _user_data: *mut c_void) {
    info!(state, "evdi crtc state");
}

fn bgra_to_rgb(src: &[u8], w: usize, h: usize, stride: usize, dst: &mut [u8]) {
    for y in 0..h {
        let row = &src[y * stride..y * stride + w * 4];
        let drow = &mut dst[y * w * 3..y * w * 3 + w * 3];
        for x in 0..w {
            let p = &row[x * 4..x * 4 + 4];
            let d = &mut drow[x * 3..x * 3 + 3];
            d[0] = p[2];
            d[1] = p[1];
            d[2] = p[0];
        }
    }
}

pub fn start(tx: FrameTx) -> Result<()> {
    // Find an already-created evdi device (adding one requires root — user does
    // it once via sudo; see README/setup notes).
    let mut device = -1;
    for i in 0..16 {
        let status = unsafe { evdi_check_device(i) };
        if status == EVDI_AVAILABLE {
            device = i;
            break;
        }
    }
    if device < 0 {
        // Try to add one; typically fails silently without root.
        let _ = unsafe { evdi_add_device() };
        for i in 0..16 {
            let status = unsafe { evdi_check_device(i) };
            if status == EVDI_AVAILABLE {
                device = i;
                break;
            }
        }
    }
    if device < 0 {
        bail!(
            "no evdi device available. Create one as root first:\n  \
             echo 1 | sudo tee /sys/devices/evdi/add"
        );
    }
    info!(device, "found evdi device");

    let handle = unsafe { evdi_open(device) };
    if handle.is_null() {
        bail!("evdi_open({}) failed — check /dev/dri/card* permissions", device);
    }

    let edid = edid_1080p();
    unsafe { evdi_connect(handle, edid.as_ptr(), edid.len() as u32, 0) };
    info!("evdi_connect sent EDID (128 bytes)");

    let state = Box::new(State {
        tx,
        handle,
        width: 0,
        height: 0,
        stride: 0,
        buffer_id: 1,
        bgra: Vec::new(),
        rgb: Vec::new(),
        registered: false,
        pending_update: false,
    });
    let state_ptr = Box::into_raw(state);

    // Rust 2021 disjoint captures won't let us move raw pointers into a thread
    // closure even through a Send newtype (it decomposes to fields). Shuttle
    // them across as `usize`, which is trivially Send, and cast back inside.
    let handle_addr = handle as usize;
    let state_addr = state_ptr as usize;
    thread::Builder::new()
        .name("evdi-capture".into())
        .spawn(move || {
            let h = handle_addr as *mut c_void;
            let s = state_addr as *mut State;
            run_loop(h, s);
            unsafe {
                evdi_disconnect(h);
                evdi_close(h);
                drop(Box::from_raw(s));
            }
        })
        .context("spawn evdi thread")?;
    Ok(())
}

fn run_loop(handle: *mut c_void, state_ptr: *mut State) {
    let mut ctx = EvdiEventContext {
        dpms: Some(on_dpms),
        mode_changed: Some(on_mode_changed),
        update_ready: Some(on_update_ready),
        crtc_state: Some(on_crtc_state),
        cursor_set: None,
        cursor_move: None,
        ddcci: None,
        user_data: state_ptr as *mut c_void,
    };

    let fd = unsafe { evdi_get_event_ready(handle) };
    info!(fd, "evdi event fd");

    loop {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, 10) };
        if rc < 0 {
            let errno = std::io::Error::last_os_error();
            if errno.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            error!(%errno, "poll failed, stopping evdi loop");
            return;
        }
        if rc > 0 && (pfd.revents & libc::POLLIN) != 0 {
            unsafe { evdi_handle_events(handle, &mut ctx) };
        }

        // Pump next frame request if we have a registered buffer and no outstanding request.
        let state = unsafe { &mut *state_ptr };
        if state.registered && !state.pending_update {
            state.pending_update = true;
            let ready = unsafe { evdi_request_update(state.handle, state.buffer_id) };
            if ready {
                // Already ready: deliver inline without waiting for event.
                on_update_ready(state.buffer_id, state_ptr as *mut c_void);
            }
        }
    }
}
