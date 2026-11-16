#!/usr/bin/env bash
set -euo pipefail

REPO="HamidRezaAshkiyan/clop-rs"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

echo "Installing clop-rs from $REPO..."
LATEST_TAG="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/')"
if [ -z "$LATEST_TAG" ]; then
    LATEST_TAG="v0.1.0"
fi

TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

TAR_URL="https://github.com/$REPO/releases/download/$LATEST_TAG/clop-rs-${LATEST_TAG}-macos-arm64.tar.gz"
echo "Downloading $TAR_URL..."
curl -fL "$TAR_URL" -o "$TMP_DIR/clop-rs.tar.gz"
tar -xzf "$TMP_DIR/clop-rs.tar.gz" -C "$TMP_DIR"

EXTRACTED_DIR="$TMP_DIR/clop-rs-${LATEST_TAG}-macos-arm64"
if [ -d "$EXTRACTED_DIR/clop-rs.app" ] && [ -w "/Applications" ]; then
    rm -rf "/Applications/clop-rs.app"
    cp -R "$EXTRACTED_DIR/clop-rs.app" "/Applications/clop-rs.app"
    echo "✓ Installed /Applications/clop-rs.app"
fi

if [ -w "$INSTALL_DIR" ]; then
    cp "$EXTRACTED_DIR/clop-rs" "$INSTALL_DIR/clop-rs"
    chmod +x "$INSTALL_DIR/clop-rs"
    echo "✓ Installed CLI binary to $INSTALL_DIR/clop-rs"
else
    sudo cp "$EXTRACTED_DIR/clop-rs" "$INSTALL_DIR/clop-rs"
    sudo chmod +x "$INSTALL_DIR/clop-rs"
    echo "✓ Installed CLI binary to $INSTALL_DIR/clop-rs"
fi

echo "Done! Run 'clop-rs' or open /Applications/clop-rs.app to start."
