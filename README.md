# Ferrite

Use an Android tablet as an external display + pen tablet for a Linux host. Inspired by Duet.

The Linux host captures a real or virtual screen, encodes it as low-latency H.264 (VA-API), and streams it to the tablet; the tablet returns multi-touch + pen input back to the host as uinput events.

Status: alpha. Works on Pop!\_OS / COSMIC with an AMD GPU and a Samsung S-Pen tablet; other setups unproven.

## Features

- **Mirror mode** — stream an existing output picked via `xdg-desktop-portal` + PipeWire.
- **Virtual monitor mode** (`FERRITE_MODE=virtual`) — host creates an `evdi`-backed second monitor sized to the tablet's resolution; drag windows onto it to stream.
- **Multi-touch + pen** — two uinput devices (touchscreen + pen tablet with pressure, proximity, eraser). Wayland `tablet_v2` apps see the pen as a real tablet.
- **Three transports**, auto-selected on the tablet:
  - **AOA (Android Open Accessory)** — plug USB cable, the Android app launches automatically via intent, host enters accessory mode, video + input flow over USB bulk endpoints. No developer mode, no `adb`. Default.
  - **USB via `adb reverse`** — `adb reverse tcp:7543 tcp:7543`, app connects to `127.0.0.1:7543`. Useful on emulators.
  - **Wi-Fi** — QR-code pairing. Opt-in from the tablet.
- **Tray** (`ferrite-tray`) — owns the host child, exposes enable/disable + mirror/virtual toggle. Autostart at login.

## Prerequisites

### Linux host

System packages:

```bash
sudo apt install \
  libpipewire-0.3-dev libspa-0.2-dev libclang-dev clang \
  libavcodec-dev libavformat-dev libavutil-dev mesa-va-drivers \
  evdi-dkms libevdi-dev
```

Additionally, JDK 17+ for APK builds (`sudo apt install openjdk-17-jdk`).

Rust toolchain via [rustup](https://rustup.rs/). Android targets for cross-compiling the JNI library:

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
```

Android NDK (API level 34+) on `PATH`; the NDK's `toolchains/llvm/prebuilt/linux-x86_64/bin` must have the `aarch64-linux-android34-clang` / `armv7a-linux-androideabi34-clang` / `x86_64-linux-android34-clang` wrappers. See `.cargo/config.toml` for the exact linker names.

### Virtual monitor (evdi)

Load the module once per boot and create a virtual card:

```bash
sudo modprobe evdi
echo 1 | sudo tee /sys/devices/evdi/add
```

A new `cardN` will appear in `/dev/dri/`. COSMIC will see it as a "22.6\" DVI-I-N external display" after the first client connect.

### AOA udev rule

For the AOA transport to work without `sudo`, install the udev rule at [`packaging/51-ferrite-aoa.rules`](packaging/51-ferrite-aoa.rules) — the `install.sh` script below does this for you.

## Build

```bash
# host + tray + ui + APK (with JNI .so for arm64 + x86_64), push to connected device
./dev.sh

# or individually
cargo build --release                  # all Rust crates
./build-android.sh                     # APK (debug)
./build-android.sh release             # APK (release)
```

The APK lands at `android-app/app/build/outputs/apk/debug/app-debug.apk`. Install with `adb install -r`.

## Install

```bash
cargo build --release
./packaging/install.sh
```

This copies the three binaries into `~/.local/bin`, adds an XDG autostart entry, and installs the AOA udev rule (needs `sudo` for the udev step).

For a systemd-managed tray instead of XDG autostart:

```bash
install -m 0644 packaging/systemd/ferrite-tray.service ~/.config/systemd/user/
systemctl --user enable --now ferrite-tray.service
```

## Run

The tray owns `ferrite-host` and exposes enable/disable + mirror/virtual toggle in the menu. Normally this just autostarts at login.

```bash
./target/release/ferrite-tray          # normal entry point
./target/release/ferrite-ui            # control panel (spawned by tray "Open Panel")
./target/release/ferrite-host          # daemon; normally owned by the tray
```

Environment variables read by `ferrite-host`:

| Variable | Default | Meaning |
|---|---|---|
| `FERRITE_MODE` | `mirror` | `mirror` or `virtual`. Virtual requires evdi. |
| `FERRITE_AOA` | on | Set to `0` to disable the AOA listener. |
| `FERRITE_PEN_MIRROR` | on | Set to `0` to stop mirroring pen position onto the touch device (useful with tablet_v2 apps like Krita). |
| `FERRITE_H264_DUMP` | unset | Path to write Annex-B H.264 captures for debugging. |
| `RUST_LOG` | `error` | Standard `tracing_subscriber` filter — try `info` or `ferrite_host=debug`. |

## Layout

Cargo workspace:

- `core/` — shared wire protocol (bincode + serde)
- `host/` — Linux daemon (capture, encode, uinput)
- `tray/` — StatusNotifierItem, owns the host child
- `ui/` — libcosmic control panel (throwaway)
- `android-jni/` — `cdylib` loaded by the Android app
- `android-app/` — Kotlin app (MainActivity + accessory activities)

Internal dev docs live in [`CLAUDE.md`](CLAUDE.md); a ledger of what works / what doesn't is in [`STATUS.md`](STATUS.md).
