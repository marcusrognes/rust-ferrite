#!/usr/bin/env bash
# Install Ferrite for the current user: copies the three release binaries
# into ~/.local/bin, drops an XDG autostart .desktop entry, and writes the
# AOA udev rule (the udev step needs sudo).
#
# Idempotent — safe to re-run after a rebuild to refresh the binaries.

set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${HOME}/.local/bin"
AUTOSTART="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"
UDEV_DIR="/etc/udev/rules.d"

BINS=(ferrite-tray ferrite-host ferrite-ui)

if [[ ! -x "${REPO}/target/release/ferrite-tray" ]]; then
    echo "error: release binaries missing. Run \`cargo build --release\` first." >&2
    exit 1
fi

mkdir -p "$BIN" "$AUTOSTART"

echo "==> installing binaries to $BIN"
for b in "${BINS[@]}"; do
    install -m 0755 "${REPO}/target/release/$b" "${BIN}/$b"
    echo "    $BIN/$b"
done

echo "==> installing autostart entry"
install -m 0644 "${REPO}/packaging/ferrite-tray.desktop" \
    "${AUTOSTART}/ferrite-tray.desktop"
echo "    ${AUTOSTART}/ferrite-tray.desktop"

echo "==> installing AOA udev rule (sudo required)"
sudo install -m 0644 "${REPO}/packaging/51-ferrite-aoa.rules" \
    "${UDEV_DIR}/51-ferrite-aoa.rules"
sudo udevadm control --reload
sudo udevadm trigger
echo "    ${UDEV_DIR}/51-ferrite-aoa.rules"

echo
echo "Done. Log out + log in (or run $BIN/ferrite-tray directly) to start the tray."
echo "If \$HOME/.local/bin isn't on PATH, add it to your shell profile."
