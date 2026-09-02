#!/usr/bin/env bash
# Build Sori.app: SwiftUI menu bar shell + Rust core/CLI.
#   scripts/bundle.sh            -> target/Sori.app
#   scripts/bundle.sh --debug    -> use the debug Rust build
set -euo pipefail

cd "$(dirname "$0")/.."

command -v python3 >/dev/null || { echo "python3 is required to generate dependency notices" >&2; exit 1; }
python3 scripts/generate-third-party.py

PROFILE=release
if [[ "${1:-}" == "--debug" ]]; then
  PROFILE=debug
fi

# 1. Rust core + CLI
if [[ "$PROFILE" == "release" ]]; then
  cargo build -p sori-app --bin sori-core --bin sori-cli --release --locked
else
  cargo build -p sori-app --bin sori-core --bin sori-cli --locked
fi

# 2. SwiftUI shell (single file, no Xcode project needed)
mkdir -p target/sori-menu
xcrun swiftc -O -parse-as-library \
  -target arm64-apple-macos14.0 \
  -framework SwiftUI -framework AppKit -framework Carbon -framework ServiceManagement -framework UserNotifications \
  macos/SoriMenu.swift -o target/sori-menu/Sori

# 3. Bundle
APP=target/Sori.app
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/sori-menu/Sori          "$APP/Contents/MacOS/Sori"
cp "target/$PROFILE/sori-core"    "$APP/Contents/MacOS/sori-core"
cp "target/$PROFILE/sori-cli"     "$APP/Contents/MacOS/sori-cli"
cp macos/AppIcon.icns "$APP/Contents/Resources/AppIcon.icns"
cp THIRD_PARTY_NOTICES.txt "$APP/Contents/Resources/THIRD_PARTY_NOTICES.txt"

VERSION=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)"/\1/')

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Sori</string>
  <key>CFBundleDisplayName</key><string>Sori</string>
  <key>CFBundleIdentifier</key><string>com.sori.recorder</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>Sori</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>LSMinimumSystemVersion</key><string>14.4</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSMicrophoneUsageDescription</key>
  <string>Sori records your voice during meetings.</string>
  <key>NSAudioCaptureUsageDescription</key>
  <string>Sori records system audio while a recording is active.</string>
  <key>NSAppleEventsUsageDescription</key>
  <string>Sori uses this only to install its command-line tool with your permission.</string>
</dict>
</plist>
PLIST

# Ad-hoc signature is enough for TCC prompts on the local machine.
codesign --force --deep --sign - "$APP"

echo "built: $APP ($PROFILE)"
du -sh "$APP" | cut -f1
