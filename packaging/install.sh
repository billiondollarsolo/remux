#!/usr/bin/env bash
#
# Install script for Remux
#
# Builds remux and remuxd, copies them to ~/.cargo/bin/, and installs
# the appropriate system service (systemd on Linux, launchd on macOS).
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALL_DIR="${HOME}/.cargo/bin"

echo "=== Remux Installer ==="

# --- Build ---
echo ""
echo "Building remux and remuxd..."
cargo build --release --manifest-path "$PROJECT_ROOT/Cargo.toml" -p remux-cli -p remux-daemon

# --- Install binaries ---
echo ""
echo "Installing binaries to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR"
cp "$PROJECT_ROOT/target/release/remux" "$INSTALL_DIR/remux"
cp "$PROJECT_ROOT/target/release/remuxd" "$INSTALL_DIR/remuxd"
chmod +x "$INSTALL_DIR/remux" "$INSTALL_DIR/remuxd"
echo "  Installed: $INSTALL_DIR/remux"
echo "  Installed: $INSTALL_DIR/remuxd"

# --- Install system service ---
OS="$(uname -s)"
echo ""
echo "Detected OS: $OS"

case "$OS" in
    Linux)
        echo "Installing systemd user service..."
        SYSTEMD_DIR="${HOME}/.config/systemd/user"
        mkdir -p "$SYSTEMD_DIR"
        cp "$SCRIPT_DIR/remuxd.service" "$SYSTEMD_DIR/remuxd.service"
        systemctl --user daemon-reload
        echo "  Installed: $SYSTEMD_DIR/remuxd.service"
        echo ""
        echo "To enable and start the daemon:"
        echo "  systemctl --user enable remuxd"
        echo "  systemctl --user start remuxd"
        ;;
    Darwin)
        echo "Installing launchd plist..."
        PLIST_SRC="$SCRIPT_DIR/com.remux.daemon.plist"
        PLIST_DST="${HOME}/Library/LaunchAgents/com.remux.daemon.plist"
        # Replace placeholder with actual username
        sed "s|%USERNAME%|$(whoami)|g" "$PLIST_SRC" > "$PLIST_DST"
        echo "  Installed: $PLIST_DST"
        echo ""
        echo "To load and start the daemon:"
        echo "  launchctl load $PLIST_DST"
        ;;
    *)
        echo "Unsupported OS: $OS"
        echo "You can manually start the daemon with: remuxd"
        ;;
esac

echo ""
echo "=== Installation complete ==="
echo "Add $INSTALL_DIR to your PATH if it is not already there."
