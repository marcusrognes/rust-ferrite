#!/usr/bin/env bash
# One-shot: rebuild host + ui + android APK, then push to the connected
# Android device and launch. Skips install/launch if no device is attached.
#
# Usage: ./dev.sh [--no-install]

set -euo pipefail
cd "$(dirname "$0")"

print_next_steps() {
    cat <<'EOF'

==> next steps:

  # mirror an existing display (default):
  ./target/release/ferrite-host

  # virtual monitor mode (needs evdi loaded):
  sudo modprobe evdi && echo 1 | sudo tee /sys/devices/evdi/add
  FERRITE_MODE=virtual ./target/release/ferrite-host

  # control panel UI (spawns host as child):
  ./target/release/ferrite-ui

  # USB transport (instead of Wi-Fi):
  adb reverse tcp:7543 tcp:7543

EOF
}
trap 'rc=$?; [[ $rc -eq 0 ]] && print_next_steps' EXIT

NO_INSTALL=0
for arg in "$@"; do
    case "$arg" in
        --no-install) NO_INSTALL=1 ;;
        *) echo "unknown flag: $arg"; exit 2 ;;
    esac
done

echo "==> killing running ferrite-host (if any)"
pkill -x ferrite-host 2>/dev/null || true

echo "==> cargo build -p ferrite-host --release"
cargo build -p ferrite-host --release

echo "==> cargo build -p ferrite-ui --release"
cargo build -p ferrite-ui --release

echo "==> ./build-android.sh"
./build-android.sh

if [[ "$NO_INSTALL" == "1" ]]; then
    echo "==> --no-install set, stopping here"
    exit 0
fi

ADB="${ADB:-$HOME/Android/Sdk/platform-tools/adb}"
if [[ ! -x "$ADB" ]]; then
    if command -v adb >/dev/null 2>&1; then
        ADB=adb
    else
        echo "==> adb not found at $ADB or on PATH; skipping install"
        exit 0
    fi
fi

DEVICE_LINES=$("$ADB" devices | tail -n +2 | awk '$2=="device"{print $1}')
if [[ -z "$DEVICE_LINES" ]]; then
    echo "==> no adb device attached — skipping install/launch"
    echo "    (USB debugging on? \`$ADB devices\` to check)"
    exit 0
fi

APK="android-app/app/build/outputs/apk/debug/app-debug.apk"
echo "==> installing $APK"
"$ADB" install -r "$APK"

echo "==> relaunching com.ferrite/.MainActivity"
"$ADB" shell am force-stop com.ferrite
"$ADB" shell am start -n com.ferrite/.MainActivity >/dev/null

echo "==> done."
