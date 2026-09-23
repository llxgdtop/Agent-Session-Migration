#!/bin/bash
# Package hub-app as a macOS .app bundle (double-click to launch).
# Usage: cargo build --release, then ./scripts/build-app.sh
# The app icon comes from assets/icon.icns (see scripts/gen_icon.py).
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/hub-app
APP=dist/AgentSessionHub.app
ICON=assets/icon.icns

[ -f "$BIN" ] || { echo "未找到 $BIN,请先 cargo build --release"; exit 1; }
[ -f "$ICON" ] || { echo "未找到 $ICON,请先运行 python3 scripts/gen_icon.py"; exit 1; }

mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/hub-app"
cp "$ICON" "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>              <string>Agent Session Migration</string>
    <key>CFBundleDisplayName</key>       <string>Agent Session Migration</string>
    <key>CFBundleIdentifier</key>        <string>com.llxgdtop.sessionmanage</string>
    <key>CFBundleExecutable</key>        <string>hub-app</string>
    <key>CFBundleIconFile</key>          <string>icon</string>
    <key>CFBundlePackageType</key>       <string>APPL</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key>           <string>1</string>
    <key>NSHighResolutionCapable</key>   <true/>
    <key>LSMinimumSystemVersion</key>    <string>11.0</string>
</dict>
</plist>
PLIST

echo "✅ 已打包:$PWD/$APP"
echo "   双击打开,或:open $APP"
