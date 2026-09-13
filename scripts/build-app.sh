#!/bin/bash
# 打包 hub-app 为 macOS .app(双击启动)。
# 用法:先 cargo build --release,再 ./scripts/build-app.sh
set -euo pipefail
cd "$(dirname "$0")/.."

BIN=target/release/hub-app
APP=dist/AgentSessionHub.app

[ -f "$BIN" ] || { echo "未找到 $BIN,请先 cargo build --release"; exit 1; }

mkdir -p "$APP/Contents/MacOS"
cp "$BIN" "$APP/Contents/MacOS/hub-app"

cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>              <string>Agent Session Hub</string>
    <key>CFBundleDisplayName</key>       <string>Agent Session Hub</string>
    <key>CFBundleIdentifier</key>        <string>com.llxgdtop.sessionmanage</string>
    <key>CFBundleExecutable</key>        <string>hub-app</string>
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
