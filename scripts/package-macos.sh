#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:-v0.1.0}"
ARCH="${2:-arm64}"
ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DIST_DIR="$ROOT_DIR/dist"
BIN_PATH="$ROOT_DIR/target/release/clop-rs"
CLI_BIN_PATH="$ROOT_DIR/target/release/clop-cli"

if [ ! -f "$BIN_PATH" ] || [ ! -f "$CLI_BIN_PATH" ]; then
    echo "Building release binaries..."
    cargo build --release --manifest-path "$ROOT_DIR/Cargo.toml"
fi

rm -rf "$DIST_DIR"
mkdir -p "$DIST_DIR"

APP_NAME="clop-rs.app"
APP_DIR="$DIST_DIR/$APP_NAME"
mkdir -p "$APP_DIR/Contents/MacOS"
mkdir -p "$APP_DIR/Contents/Resources"

cp "$BIN_PATH" "$APP_DIR/Contents/MacOS/clop-rs"
chmod +x "$APP_DIR/Contents/MacOS/clop-rs"

# Generate Info.plist
cat > "$APP_DIR/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleExecutable</key>
    <string>clop-rs</string>
    <key>CFBundleIconFile</key>
    <string>AppIcon</string>
    <key>CFBundleIdentifier</key>
    <string>com.hamidrezaashkiyan.clop-rs</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>clop-rs</string>
    <key>CFBundleDisplayName</key>
    <string>clop-rs</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION#v}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION#v}</string>
    <key>LSMinimumSystemVersion</key>
    <string>11.0</string>
    <key>LSUIElement</key>
    <true/>
    <key>NSHighResolutionCapable</key>
    <true/>
</dict>
</plist>
EOF

# Generate AppIcon.icns from assets/logo.png (falling back to procedural icon if missing)
ICONSET_DIR="$DIST_DIR/AppIcon.iconset"
mkdir -p "$ICONSET_DIR"
LOGO_SRC="$ROOT_DIR/assets/logo.png"
if [ -f "$LOGO_SRC" ]; then
    sips -s format png -z 1024 1024 "$LOGO_SRC" --out "$ICONSET_DIR/icon_512x512@2x.png" >/dev/null
else
    python3 - "$ICONSET_DIR/icon_512x512@2x.png" <<'PY'
import sys, struct, zlib

out_path = sys.argv[1]
w, h = 256, 256
raw = bytearray()
for y in range(h):
    raw.append(0)
    for x in range(w):
        dx = min(x, w - 1 - x)
        dy = min(y, h - 1 - y)
        if dx < 20 and dy < 20 and (20 - dx)**2 + (20 - dy)**2 > 400:
            raw.extend((0, 0, 0, 0))
            continue
        in_bolt = abs((x - 128) + (y - 128) * 0.4) < 22 and 56 < y < 200
        if in_bolt:
            raw.extend((52, 211, 153, 255))
        else:
            raw.extend((24, 30, 42, 255))

def chunk(t, d):
    return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xffffffff)

png = (
    b"\x89PNG\r\n\x1a\n"
    + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
    + chunk(b"IDAT", zlib.compress(bytes(raw), 6))
    + chunk(b"IEND", b"")
)
open(out_path, "wb").write(png)
PY
fi

for sz in 16 32 128 256 512; do
    sips -s format png -z $sz $sz "$ICONSET_DIR/icon_512x512@2x.png" --out "$ICONSET_DIR/icon_${sz}x${sz}.png" >/dev/null
    sz2=$((sz * 2))
    sips -s format png -z $sz2 $sz2 "$ICONSET_DIR/icon_512x512@2x.png" --out "$ICONSET_DIR/icon_${sz}x${sz}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET_DIR" -o "$APP_DIR/Contents/Resources/AppIcon.icns" || true
rm -rf "$ICONSET_DIR"

# Sign the app bundle (Developer ID when APPLE_SIGNING_IDENTITY is set, otherwise ad-hoc)
if [ -n "${APPLE_SIGNING_IDENTITY:-}" ]; then
    codesign --force --deep --options runtime --timestamp --sign "$APPLE_SIGNING_IDENTITY" "$APP_DIR"
else
    codesign --force --deep --sign - "$APP_DIR" 2>/dev/null || true
fi

# 1. Build macOS .pkg installer (installs /Applications/clop-rs.app, /usr/local/bin/clop-rs, and Finder Quick Action)
PKG_ROOT="$DIST_DIR/pkg_root"
PKG_SCRIPTS="$DIST_DIR/pkg_scripts"
mkdir -p "$PKG_ROOT/Applications" "$PKG_ROOT/usr/local/bin" "$PKG_SCRIPTS"
cp -R "$APP_DIR" "$PKG_ROOT/Applications/"
cp "$BIN_PATH" "$PKG_ROOT/usr/local/bin/clop-rs"
chmod +x "$PKG_ROOT/usr/local/bin/clop-rs"
if [ -f "$CLI_BIN_PATH" ]; then
    cp "$CLI_BIN_PATH" "$PKG_ROOT/usr/local/bin/clop-cli"
    chmod +x "$PKG_ROOT/usr/local/bin/clop-cli"
fi

cat > "$PKG_SCRIPTS/postinstall" <<'POST'
#!/usr/bin/env bash
xattr -cr "/Applications/clop-rs.app" "/usr/local/bin/clop-rs" "/usr/local/bin/clop-cli" 2>/dev/null || true
CONSOLE_USER="$(stat -f "%Su" /dev/console 2>/dev/null || true)"
if [ -n "$CONSOLE_USER" ] && [ "$CONSOLE_USER" != "root" ]; then
    sudo -u "$CONSOLE_USER" "/usr/local/bin/clop-rs" context-menu --enable 2>/dev/null || true
    sudo -u "$CONSOLE_USER" open "/Applications/clop-rs.app" 2>/dev/null || true
fi
exit 0
POST
chmod +x "$PKG_SCRIPTS/postinstall"

PKG_PATH="$DIST_DIR/clop-rs-${VERSION}-macos-${ARCH}.pkg"
pkgbuild \
    --root "$PKG_ROOT" \
    --scripts "$PKG_SCRIPTS" \
    --install-location "/" \
    --identifier "com.hamidrezaashkiyan.clop-rs.pkg" \
    --version "${VERSION#v}" \
    "$PKG_PATH"
rm -rf "$PKG_ROOT" "$PKG_SCRIPTS"

# 2. Build macOS .dmg installer (if allowed by environment)
DMG_STAGING="$DIST_DIR/dmg_staging"
mkdir -p "$DMG_STAGING"
cp -R "$APP_DIR" "$DMG_STAGING/"
ln -s /Applications "$DMG_STAGING/Applications"
DMG_PATH="$DIST_DIR/clop-rs-${VERSION}-macos-${ARCH}.dmg"
hdiutil create -volname "clop-rs ${VERSION}" -srcfolder "$DMG_STAGING" -ov -format UDZO "$DMG_PATH" 2>/dev/null || true
rm -rf "$DMG_STAGING"

# 3. Build .tar.gz archive (contains CLI binary + .app bundle + README + LICENSE)
TAR_STAGING="$DIST_DIR/clop-rs-${VERSION}-macos-${ARCH}"
mkdir -p "$TAR_STAGING"
cp "$BIN_PATH" "$TAR_STAGING/clop-rs"
if [ -f "$CLI_BIN_PATH" ]; then
    cp "$CLI_BIN_PATH" "$TAR_STAGING/clop-cli"
fi
cp -R "$APP_DIR" "$TAR_STAGING/"
cp "$ROOT_DIR/README.md" "$ROOT_DIR/LICENSE" "$TAR_STAGING/"
tar -czf "$DIST_DIR/clop-rs-${VERSION}-macos-${ARCH}.tar.gz" -C "$DIST_DIR" "clop-rs-${VERSION}-macos-${ARCH}"
rm -rf "$TAR_STAGING" "$APP_DIR"

if [ -n "${APPLE_NOTARY_KEYCHAIN_PROFILE:-}" ]; then
    echo "Notarizing .pkg and .dmg with profile ${APPLE_NOTARY_KEYCHAIN_PROFILE}..."
    xcrun notarytool submit "$PKG_PATH" --keychain-profile "$APPLE_NOTARY_KEYCHAIN_PROFILE" --wait
    xcrun stapler staple "$PKG_PATH" || true
    if [ -f "$DMG_PATH" ]; then
        xcrun notarytool submit "$DMG_PATH" --keychain-profile "$APPLE_NOTARY_KEYCHAIN_PROFILE" --wait
        xcrun stapler staple "$DMG_PATH" || true
    fi
fi

echo "Packaged release artifacts in $DIST_DIR:"
ls -lh "$DIST_DIR"
