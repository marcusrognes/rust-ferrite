# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Purpose

Duet-like system: use an Android device as an external display / pen tablet for a Linux host. Host captures screen + encodes + streams video to Android; Android returns touch/pen input.

## Workspace layout

Cargo workspace (`resolver = "2"`) with three crates plus an Android app:

- **`core/`** (`ferrite-core`, lib) — shared wire protocol. `HostMessage` (video frames, ping) and `ClientMessage` (touch events, pong), serialized via `bincode` + `serde`. Both host and Android link this crate; keep enum variants in sync on both sides.
- **`host/`** (`ferrite-host`, bin) — Linux-side daemon. `tokio` runtime, `tracing` logging. Currently a stub (`main.rs` just logs and exits). Future: screen capture → encode → stream to client, receive input events.
- **`android-jni/`** (`ferrite-android`, `cdylib`) — native library loaded by the Android app over JNI (`jni` 0.21). Exports `Java_com_ferrite_FerriteLib_*` symbols. Produces `.so` files only.
- **`android-app/`** — Android Gradle project (AGP 8.7.0, Kotlin 1.9.25, Gradle 8.9 via wrapper). Package `com.ferrite`, `MainActivity` calls `FerriteLib.connect()` on startup. Builds APK that bundles `libferrite_android.so` for both `arm64-v8a` (device) and `x86_64` (emulator) under `app/src/main/jniLibs/<abi>/`. `local.properties` (gitignored) points at `~/Android/Sdk`.

## Build / dev commands

```bash
# Build everything for host platform
cargo build                       # debug
cargo build --release

# Run host binary
cargo run -p ferrite-host

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

## Protocol invariants

`HostMessage` / `ClientMessage` in `core/src/lib.rs` are the wire format. `bincode` is not self-describing, so any variant reorder or field change is a breaking protocol change — bump versions on both host and Android together, or add a version handshake before shipping.

## Current state

Skeleton end-to-end. `host/src/main.rs` logs startup and exits (`TODO: screen capture, encode, stream`). `android-jni/src/lib.rs` has one stub `connect()` returning `"ferrite-android ready"`, which `MainActivity` renders as centered text to prove the JNI path works. No transport, no capture pipeline, no input routing yet.
