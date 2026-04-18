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

  # Start the tray (owns host lifecycle; stays running in system tray):
  ./target/release/ferrite-tray

  # From the tray menu: "Open Panel" opens ferrite-ui on demand.
  # Mode is toggled via the tray menu and persists to ~/.config/ferrite/tray.ron.

  # Run host directly (no tray / no autostart), if you just want the daemon:
  ./target/release/ferrite-host                       # mirror mode
  FERRITE_MODE=virtual ./target/release/ferrite-host  # virtual monitor

  # Evdi setup (once per boot, required for virtual mode):
  sudo modprobe evdi && echo 1 | sudo tee /sys/devices/evdi/add

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

echo "==> killing running ferrite-{tray,host,ui} (if any)"
pkill -x ferrite-tray 2>/dev/null || true
pkill -x ferrite-host 2>/dev/null || true
pkill -x ferrite-ui 2>/dev/null || true

echo "==> cargo build --release (host + ui + tray)"
cargo build --release -p ferrite-host -p ferrite-ui -p ferrite-tray

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
