#!/bin/sh
#
# Build and install Styx Receiver as a macOS .app bundle with launchd autostart.
#
# The .app bundle is required so macOS can grant Accessibility permission
# to a stable application identity. The launchd agent uses
# AssociatedBundleIdentifiers to inherit that TCC grant.
#
# Usage:
#   ./dist/macos/install.sh
#
# After installation:
#   1. Open System Settings > Privacy & Security > Accessibility
#   2. Add /Applications/Styx Receiver.app and enable it
#   3. Reboot (or run: launchctl kickstart -k gui/$(id -u)/com.ghreprimand.styx-receiver)

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_DIR="$(cd "$SCRIPT_DIR/../.." && pwd)"
APP_DIR="/Applications/Styx Receiver.app"
PLIST_NAME="com.ghreprimand.styx-receiver.plist"
PLIST_DST="$HOME/Library/LaunchAgents/$PLIST_NAME"

# Build
echo "Building styx-receiver..."
cd "$REPO_DIR"
cargo build --release -p styx-receiver

# Stop existing instances
launchctl bootout "gui/$(id -u)/com.ghreprimand.styx-receiver" 2>/dev/null || true
pkill -9 styx-receiver 2>/dev/null || true
sleep 1

# Remove legacy autostart mechanisms
echo "Cleaning up legacy autostart mechanisms..."
rm -rf "/Applications/Styx Login.app"
rm -rf "/Applications/Start Styx Receiver.app"
rm -f /usr/local/bin/styx-receiver 2>/dev/null || true

# Clean fish config if it has the styx autostart block
FISH_CONFIG="$HOME/.config/fish/config.fish"
if [ -f "$FISH_CONFIG" ] && grep -q 'styx-receiver' "$FISH_CONFIG"; then
    echo "Removing styx autostart from fish config..."
    sed -i '' '/# Start styx-receiver/,/^end$/d' "$FISH_CONFIG"
fi

# Remove Styx Login from Login Items (best-effort, requires osascript)
osascript -e 'tell application "System Events" to delete login item "Styx Login"' 2>/dev/null || true

# Create .app bundle
echo "Installing to $APP_DIR..."
rm -rf "$APP_DIR"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"
cp "$SCRIPT_DIR/Info.plist" "$APP_DIR/Contents/Info.plist"
cp "$REPO_DIR/target/release/styx-receiver" "$APP_DIR/Contents/MacOS/styx-receiver"

# Codesign with styx-cert for stable identity across rebuilds.
# Create the certificate once in Keychain Access:
#   Keychain Access > Certificate Assistant > Create a Certificate
#   Name: styx-cert, Type: Self Signed Root, Certificate Type: Code Signing
SIGN_IDENTITY="styx-cert"
if security find-certificate -c "$SIGN_IDENTITY" >/dev/null 2>&1; then
    codesign -f -s "$SIGN_IDENTITY" "$APP_DIR"
else
    echo "Warning: '$SIGN_IDENTITY' certificate not found, using ad-hoc signature."
    echo "TCC may not grant Accessibility permission reliably with ad-hoc signing."
    echo "See README for instructions on creating the certificate."
    codesign -f -s - "$APP_DIR"
fi

# Install launchd agent
echo "Installing launchd agent..."
mkdir -p "$HOME/Library/LaunchAgents"
cp "$SCRIPT_DIR/styx-receiver.plist" "$PLIST_DST"

# Load and start
launchctl bootstrap "gui/$(id -u)" "$PLIST_DST"
launchctl kickstart -k "gui/$(id -u)/com.ghreprimand.styx-receiver"

echo ""
echo "Installed and running."
echo ""
echo "If this is a fresh install, grant Accessibility permission:"
echo "  System Settings > Privacy & Security > Accessibility"
echo "  Add and enable: Styx Receiver.app"
echo ""
echo "The receiver will start automatically on login."
echo "Logs: /tmp/styx-receiver.stderr.log"
