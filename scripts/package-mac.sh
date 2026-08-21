#!/usr/bin/env bash
#
# 打 macOS 安装包（universal .app + dmg）。
#
#   ./scripts/package-mac.sh            # 只打当前架构，快，本地验证用
#   ./scripts/package-mac.sh --universal # Intel + Apple Silicon 通用包，发版用
#
# 没有购买 Apple 开发者证书，所以只做 ad-hoc 签名。这能让应用跑起来，
# 但挡不住首次打开时的 Gatekeeper 提示——用户需要右键打开一次。
# 下载页上写清楚了这一步。
set -euo pipefail

cd "$(dirname "$0")/.."

UNIVERSAL=0
[ "${1:-}" = "--universal" ] && UNIVERSAL=1

VERSION=$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)
DIST="dist"
APP="$DIST/穿云.app"

step() { printf '\n\033[1;32m▸ %s\033[0m\n' "$1"; }

step "编译（release）"
if [ "$UNIVERSAL" = 1 ]; then
    rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
    cargo build --release -p cy-desktop --target aarch64-apple-darwin
    cargo build --release -p cy-desktop --target x86_64-apple-darwin
else
    cargo build --release -p cy-desktop
fi

step "组装 .app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

if [ "$UNIVERSAL" = 1 ]; then
    # lipo 把两个架构合成一个二进制，一个安装包在两种 Mac 上都能跑
    lipo -create \
        target/aarch64-apple-darwin/release/chuanyun \
        target/x86_64-apple-darwin/release/chuanyun \
        -output "$APP/Contents/MacOS/chuanyun"
else
    cp target/release/chuanyun "$APP/Contents/MacOS/chuanyun"
fi
chmod +x "$APP/Contents/MacOS/chuanyun"

sed "s/__VERSION__/$VERSION/g" packaging/macos/Info.plist.tmpl > "$APP/Contents/Info.plist"

step "生成图标"
ICONSET="$DIST/icon.iconset"
rm -rf "$ICONSET"; mkdir -p "$ICONSET"
SRC=crates/cy-desktop/ui/icon.png
for size in 16 32 64 128 256 512; do
    sips -z $size $size "$SRC" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null 2>&1
    double=$((size * 2))
    sips -z $double $double "$SRC" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null 2>&1
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/icon.icns"
rm -rf "$ICONSET"

step "签名（ad-hoc）"
codesign --force --deep --sign - "$APP"
codesign --verify --verbose "$APP" 2>&1 | sed 's/^/  /'

step "打 dmg"
# 文件名用 ASCII：CI 的 upload-artifact 会把非 ASCII 字符吞掉
# （v0.1.0 第一次发出来就变成了 "-0.1.0.dmg"）。
# .app 和卷标仍然叫「穿云」——那才是用户看得见的名字。
if [ "$UNIVERSAL" = 1 ]; then
    DMG="$DIST/chuanyun-$VERSION-macos-universal.dmg"
else
    DMG="$DIST/chuanyun-$VERSION-macos-$(uname -m).dmg"
fi
rm -f "$DMG"
STAGE="$DIST/dmg-stage"
rm -rf "$STAGE"; mkdir -p "$STAGE"
cp -R "$APP" "$STAGE/"
# 拖进 Applications 的那个快捷方式，装过 mac 软件的人都认得
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "穿云" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"

printf '\n\033[1;32m✓ 打包完成\033[0m\n'
ls -lh "$DMG" | awk '{print "  " $9 "  " $5}'
echo
echo "提醒用户第一次打开时要右键 → 打开（没有开发者签名，双击会被 Gatekeeper 拦）"
echo
