#!/bin/sh
# Build and install the ddcvolume driver (system-wide, needs sudo) and its
# helper (per user, as a LaunchAgent). `./install.sh uninstall` removes both.
set -eu

DRIVER=/Library/Audio/Plug-Ins/HAL/DDCVolume.driver
APP="$HOME/Applications/DDCVolume.app"
AGENT=local.ddcvolume.helper
PLIST="$HOME/Library/LaunchAgents/$AGENT.plist"
DOMAIN="gui/$(id -u)"

cd "$(dirname "$0")"

if [ "${1:-}" = uninstall ]; then
	launchctl bootout "$DOMAIN/$AGENT" 2>/dev/null || true
	rm -rf "$PLIST" "$APP"
	sudo rm -rf "$DRIVER"
	sudo killall coreaudiod
	exit
fi

cargo build --release
STAGE=target/bundle
rm -rf "$STAGE"

mkdir -p "$STAGE/DDCVolume.driver/Contents/MacOS"
cp driver/Info.plist "$STAGE/DDCVolume.driver/Contents/"
cp target/release/libdriver.dylib "$STAGE/DDCVolume.driver/Contents/MacOS/DDCVolume"
codesign --force --sign - "$STAGE/DDCVolume.driver"

mkdir -p "$STAGE/DDCVolume.app/Contents/MacOS"
cp helper/Info.plist "$STAGE/DDCVolume.app/Contents/"
cp target/release/ddcvolumed "$STAGE/DDCVolume.app/Contents/MacOS/"
codesign --force --sign - "$STAGE/DDCVolume.app"

launchctl bootout "$DOMAIN/$AGENT" 2>/dev/null || true

sudo rm -rf "$DRIVER"
sudo cp -R "$STAGE/DDCVolume.driver" "$DRIVER"
sudo killall coreaudiod

mkdir -p "$HOME/Applications" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
rm -rf "$APP"
cp -R "$STAGE/DDCVolume.app" "$APP"
cat > "$PLIST" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>$AGENT</string>
	<key>Program</key>
	<string>$APP/Contents/MacOS/ddcvolumed</string>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<true/>
	<key>ProcessType</key>
	<string>Interactive</string>
	<key>StandardErrorPath</key>
	<string>$HOME/Library/Logs/ddcvolumed.log</string>
</dict>
</plist>
PLIST
launchctl bootstrap "$DOMAIN" "$PLIST"
echo "installed: $DRIVER, $APP"
