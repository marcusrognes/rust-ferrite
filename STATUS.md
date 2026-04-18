# Ferrite Status

Running ledger of what works, what doesn't, and where half-finished experiments live.

## Works

- **Wi-Fi transport** (`main`) — TCP over LAN, QR-code pairing from the tablet.
- **USB transport via `adb reverse`** — plug device, run `adb reverse tcp:7543 tcp:7543`, app auto-detects `127.0.0.1:7543`.
- **Mirror mode** — captures an existing display via xdg-desktop-portal + PipeWire.
- **Virtual monitor mode** (`FERRITE_MODE=virtual`) — evdi-backed second monitor, sized to the client's screen via `Hello`. Shows in cosmic as `DVI-I-N external display`. Torn down on disconnect.
- **Multi-touch** — MT-B protocol to uinput, per-finger slot tracking.
- **Pen / stylus** — separate uinput tablet device with `ABS_PRESSURE`, proximity, eraser. Pen input also mirrored to touchscreen so non-`tablet_v2` apps see cursor movement (`FERRITE_PEN_MIRROR=0` to disable).
- **Auto-reconnect on Android** — retry loop probes USB → falls back to saved Wi-Fi → retries every ~2 s until it connects or app is closed.
- **Welcome / fullscreen UI on the tablet** — black welcome screen w/ scan-QR + forget-Wi-Fi, swaps to immersive fullscreen once frames arrive.
- **Tray + UI split** (host side) — `ferrite-tray` owns the `ferrite-host` child, exposes enable/disable + mirror/virtual toggle; `ferrite-ui` is a throwaway control panel.
- **Auto cosmic input mapping** — host writes `~/.config/cosmic/com.system76.CosmicComp/v1/input_devices` with per-client device-name entries mapped to the freshly-created evdi connector.
- **Low-latency H.264 streaming** — VAAPI hardware encoder, CQP rate mode, AUD framing, SPS/PPS at every keyframe. One access-unit per wire frame so the Android decoder gets clean MediaCodec inputs.
- **Idle skip** — xxh3 hash of captured RGB; identical frames aren't re-encoded.

## Doesn't work

- **AOA (Android Open Accessory) transport** — lives on the `aoa-experiment` branch. Handshake + video direction (host → tablet) work, but the input direction (tablet → host) desyncs on the first Hello after each reconnect. Root cause seems to be stale bytes in the USB bulk IN endpoint surviving across sessions; sync-magic preamble and host-side drain mitigate but don't fully fix. Activate with `FERRITE_AOA=1` once fixed.
- **Intra-refresh H.264** — only libx264 supports it; would mean giving up VAAPI hardware encoding. Skipped.

## Dependencies

System deps (install once): `libpipewire-0.3-dev`, `libspa-0.2-dev`, `libclang-dev`, `clang`, `libavcodec-dev`, `libavformat-dev`, `libavutil-dev`, `mesa-va-drivers`, `evdi-dkms`, `libevdi-dev`, JDK 17+.

When packaging for distribution: the AOA transport (on the experiment branch) requires a udev rule installed to `/etc/udev/rules.d/51-ferrite-aoa.rules` — can't be done from a user-mode binary, must come from the package's post-install script.

## Run

```
./target/release/ferrite-tray     # autostart this; spawns host + exposes tray
./target/release/ferrite-ui       # read-only control panel, spawned by tray "Open Panel"
./target/release/ferrite-host     # daemon, usually started by the tray
./dev.sh                          # rebuild host + ui + tray + APK, push to tablet
```
