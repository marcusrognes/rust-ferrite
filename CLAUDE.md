# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Collaboration

- **Commit after each successful change.** Default Claude Code behavior is to
  wait for explicit commit requests — for this repo, commit automatically once
  a change builds/tests/runs successfully. Commit message should describe the
  why; one logical change per commit. Don't commit half-finished work.
- **Keep `README.md` in sync with user-visible behavior.** When a change
  adds/removes a feature, changes a run command, introduces a new env var,
  changes prerequisites, or alters how transports/modes work, update
  `README.md` in the same commit. Internal refactors and bug fixes with no
  user-visible surface don't need README edits. `STATUS.md` is the running
  ledger (what works / what doesn't); `README.md` is the public-facing
  overview — both can move together when behavior changes.

## Purpose

Duet-like system: use an Android device as an external display / pen tablet for a Linux host. Host captures screen + encodes + streams video to Android; Android returns touch/pen input.

## Workspace layout

Cargo workspace (`resolver = "2"`) with five crates plus an Android app:

- **`core/`** (`ferrite-core`, lib) — shared wire protocol. `HostMessage` (video frames + ping), `ClientMessage` (Hello + Pointer + Touches + Pong), `PixelFormat { Rgba8, Jpeg, H264 }`, `TouchPoint`. Serialized via `bincode` + `serde`. Both host and Android link this crate; keep enum variants in sync on both sides — `bincode` isn't self-describing, so any variant reorder or field change is breaking.
- **`host/`** (`ferrite-host`, bin) — Linux-side daemon. `tokio` runtime, `tracing` logging. Contains the capture + encode + stream pipeline (see "Host architecture" below).
- **`tray/`** (`ferrite-tray`, bin) — the always-running process. Owns the `ferrite-host` child, publishes a StatusNotifierItem tray icon via `ksni`, spawns `ferrite-ui` on demand. Mode (mirror/virtual) is toggled from the tray menu and persisted to `$XDG_CONFIG_HOME/ferrite/tray.ron`. Expected to autostart at login.
- **`ui/`** (`ferrite-ui`, bin) — libcosmic throwaway control panel. Reads `ferrite-host`'s status JSON, shows QR + connection info + clients + touch-mapping + transport toggle. Does NOT own the host process (tray does). Close = exit panel, host keeps running.
- **`android-jni/`** (`ferrite-android`, `cdylib`) — native library loaded by the Android app over JNI (`jni` 0.21). Exports `Java_com_ferrite_FerriteLib_{connect,streamTcp,streamFd,sendPointer,sendTouches,disconnect}`. `streamTcp` is used by the Wi-Fi and `adb reverse` transports; `streamFd` takes a raw USB accessory file descriptor for AOA. Produces `.so` files only.
- **`android-app/`** — Android Gradle project (AGP 8.7.0, Kotlin 1.9.25, Gradle 8.9 via wrapper). Package `com.ferrite`. `MainActivity` uses a `SurfaceView` + `MediaCodec("video/avc")` decoder; `FerriteLib.stream(host, port, cb)` blocks a worker thread, firing `cb.onFrame(bytes, w, h, formatId)` per incoming H.264 chunk which is queued into MediaCodec input buffers. Built APK bundles `libferrite_android.so` for both `arm64-v8a` (device) and `x86_64` (emulator) under `app/src/main/jniLibs/<abi>/`. `local.properties` (gitignored) points at `~/Android/Sdk`.

## Build / dev commands

```bash
# One-shot: rebuild host + ui + APK, then push & relaunch on connected adb device.
# Skips install if no device attached. --no-install for build-only.
./dev.sh

# Build everything for host platform
cargo build                       # debug
cargo build --release

# Run host binary (default mirror mode; capture an existing monitor via portal)
cargo run -p ferrite-host --release

# Run host binary in virtual-monitor mode (requires evdi device — see below)
FERRITE_MODE=virtual cargo run -p ferrite-host --release

# Run the tray (owns host + spawns UI on demand — normal entry point)
cargo run -p ferrite-tray --release

# Run only the control panel UI (read-only view; does not start host)
cargo run -p ferrite-ui --release

# Build + package Android APK (Rust .so for arm64+x86_64, then gradle assembleDebug)
./build-android.sh                # debug APK
./build-android.sh release        # release APK

# Check / lint a single crate
cargo check -p ferrite-core
cargo clippy -p ferrite-host -- -D warnings

# Tests (no test files exist yet — these will run 0 tests currently)
cargo test                        # workspace
cargo test -p ferrite-core        # single crate
cargo test -p ferrite-core -- some_test_name  # single test
```

`build-android.sh` puts the NDK `bin/` on `PATH`, runs `cargo build -p ferrite-android` for each target triple listed in its `ABIS` map, copies the resulting `libferrite_android.so` into `android-app/app/src/main/jniLibs/<abi>/`, then runs `./gradlew assembleDebug`. APK lands at `android-app/app/build/outputs/apk/debug/app-debug.apk`.

### Android cross-compile setup

`.cargo/config.toml` pins NDK linkers for Android targets:

- `aarch64-linux-android` → `aarch64-linux-android34-clang`
- `armv7-linux-androideabi` → `armv7a-linux-androideabi34-clang`
- `x86_64-linux-android` → `x86_64-linux-android34-clang`

Those wrappers must be on `PATH` (from the Android NDK, API level 34+). The local setup uses NDK `30.0.14904198` under `~/Android/Sdk/ndk/` — override via `NDK_VER` env var in `build-android.sh` if that version changes. Rust targets: `rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android` (already installed here).

Requires JDK 17+ for Gradle. First build downloads Gradle 8.9 distribution + AGP 8.7 deps (~200 MB).

### Emulator workflow

AVD `ferrite` (Pixel Tablet, android-35, x86_64, 4 GB RAM) is configured at `~/.android/avd/ferrite.{ini,avd/}` — not in the repo. System image: `~/Android/Sdk/system-images/android-35/google_apis_playstore_tablet/x86_64/`.

```bash
# start emulator (backgrounded)
~/Android/Sdk/emulator/emulator -avd ferrite -no-snapshot-save -no-boot-anim &

# wait, install, launch
adb wait-for-device
until [[ "$(adb shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" == "1" ]]; do sleep 2; done
adb install -r android-app/app/build/outputs/apk/debug/app-debug.apk
adb shell am start -n com.ferrite/.MainActivity

# shutdown
adb emu kill
```

`adb` lives at `~/Android/Sdk/platform-tools/adb` — add to `PATH` or alias. The emulator is x86_64, so `x86_64` ABI must stay in `abiFilters` and `build-android.sh`'s `ABIS` map or `System.loadLibrary("ferrite_android")` will crash on launch. KVM access is granted to user `marcus` via ACL on `/dev/kvm` (not via group membership) — `getfacl /dev/kvm` confirms.

## Host architecture

Two capture modes, selected by `FERRITE_MODE` env var (default `mirror`):

- **`mirror`** (`capture.rs`) — xdg-desktop-portal ScreenCast + PipeWire. Portal handshake on tokio runtime (`ashpd` 0.13 w/ `screencast` + `tokio` features), then a dedicated std thread runs the libpipewire (`pipewire` 0.9) mainloop. User picks monitor/window in the compositor dialog. BGRx/BGRA/RGBx/RGBA all accepted and converted to tight RGB.
- **`virtual`** (`virtual_display.rs`) — evdi kernel module creates a virtual DRM output that COSMIC treats as a real second monitor. User drags windows onto it. Hand-written FFI to `libevdi` (see `/usr/include/evdi_lib.h`); dedicated std thread runs the event loop. Hardcoded 1920×1080@60 fake EDID (Linux FHD layout); base is 124 bytes + runtime-filled tail/checksum in `edid_1080p()`. Framebuffer is BGRA → converted to tight RGB.

Both modes publish `Arc<Frame { width, height, rgb }>` via `tokio::sync::watch` (lossy — slow consumers see only the latest frame, which is exactly what we want for live video).

Per client (any transport — TCP, `adb reverse`, or AOA), `main.rs::handle()`:
1. writes an 8-byte `SYNC_PREAMBLE` so the client can flush stale bytes, then reads `ClientMessage::Hello { name, w, h }` to learn device name + requested resolution,
2. in `virtual` mode, spawns a per-client evdi monitor sized to `(w, h)`; in `mirror` mode, uses the shared capture `watch` source,
3. creates per-client uinput devices via `input.rs::InputSink` — a multi-touch touchscreen and a pen tablet, both named after the client so COSMIC can remap them. In virtual mode, also writes a `~/.config/cosmic/com.system76.CosmicComp/v1/input_devices` entry pinning them to the freshly-created evdi connector,
4. spawns an `ffmpeg` subprocess (`h264_stream.rs::H264Encoder`) tuned for low-latency VA-API: `h264_vaapi` with `-rc_mode CQP -qp 22 -quality 7 -g fps/2 -bf 0 -async_depth 1 -aud 1 -bsf:v dump_extra=freq=keyframe`. CQP keeps encoder pipeline depth minimal; AUDs make access units splittable; SPS/PPS at every IDR lets the decoder resync without a full GOP,
5. `tokio::select!`s three halves: RGB watch → ffmpeg stdin, ffmpeg stdout → length-prefixed `VideoFrame { format: H264 }` bincode frames on the socket, and socket → `ClientMessage` decoding → `InputSink::pointer` / `InputSink::touches` dispatch,
6. On drop, `H264Encoder`'s `kill_on_drop(true)` SIGKILLs ffmpeg and the evdi handle's Drop tears the virtual monitor down, so disconnect fully unplugs the client from both video and input sides.

Numbers on this hardware (AMD GPU, emulator client): ~5 ms encode/frame (VA-API), ~500 KB/s wire, 30–60 fps rendered (emulator does software H.264 decode via MediaCodec SwVideoDecoder — on a real device with HW decoder it pegs 60 fps).

### Virtual monitor setup

The `.deb` package handles this automatically: it pulls in `evdi-dkms`, drops `/lib/modules-load.d/ferrite.conf` (autoloads evdi at boot), and enables `ferrite-evdi-add.service` — a oneshot that writes `1 > /sys/devices/evdi/add` after `systemd-modules-load`, creating a `/dev/dri/cardN`. Host reuses that card; no per-boot manual step.

For source / `install.sh` installs, do it by hand once per boot:

```bash
sudo apt install evdi-dkms libevdi-dev
sudo modprobe evdi
echo 1 | sudo tee /sys/devices/evdi/add
```

Confirm a new `cardN` appears in `/dev/dri/`, then `FERRITE_MODE=virtual` can find it via `evdi_check_device`. Tear down: `echo 1 | sudo tee /sys/devices/evdi/remove_all` or reboot.

Users enable/position the display in **COSMIC Settings → Displays** (appears as "22.6\" DVI-I-1 external display" / "Linux FHD" from our EDID). Dragging windows to that output makes them stream to the Android client.

### H.264 dump for debugging

Set `FERRITE_H264_DUMP=/tmp/capture.h264` to spawn a second ffmpeg inline with the capture thread that writes Annex-B H.264 to that file (independent of the per-client streaming encoder). Verify with `ffplay /tmp/capture.h264`. Useful for isolating capture vs. streaming bugs.

### Host system dependencies

- `libpipewire-0.3-dev`, `libspa-0.2-dev` — pipewire crate 0.9
- `libclang-dev`, `clang` — pipewire's bindgen
- `libturbojpeg0-dev`, `nasm` — *no longer used at runtime*, but listed here because `turbojpeg` crate was wired in then replaced; keep if you plan to swap back
- `libavcodec-dev`, `libavformat-dev`, `libavutil-dev`, `mesa-va-drivers` — ffmpeg H.264 via VA-API (h264_vaapi encoder)
- `evdi-dkms`, `libevdi-dev` — virtual monitor mode
- JDK 17+ for Gradle (APK builds)

## Current state

End-to-end live-video mirror + virtual-monitor working. Host captures (portal or evdi) → RGB → ffmpeg `h264_vaapi` → transport (TCP / `adb reverse` / AOA). Android reads chunks → MediaCodec → SurfaceView render. Input path is fully wired: Android sends `ClientMessage::{Pointer, Touches}`, host dispatches to per-client uinput devices (`input.rs::InputSink`: multi-touch MT-B touchscreen + pen tablet with pressure/proximity/eraser). Pen is mirrored onto the touch device by default so non-`tablet_v2` apps see cursor movement (`FERRITE_PEN_MIRROR=0` to disable). See `STATUS.md` for the current what-works ledger.
