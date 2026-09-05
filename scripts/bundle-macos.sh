#!/bin/sh
# Wrap a kmplify-node binary built with --features gui into "KMPLIFY Node.app"
# and a .dmg, the way a Mac expects to install a windowed program.
#
#   scripts/bundle-macos.sh <binary> <version> <output.dmg>
#
# The bundle's executable is a two-line launcher that runs the binary with
# `gui`, so double-clicking opens the window while the same binary inside
# the bundle stays the full CLI:
#
#   ln -s "/Applications/KMPLIFY Node.app/Contents/MacOS/kmplify-node" /usr/local/bin/kmplify-node
#
# Ad-hoc signed (Apple Silicon refuses to start an unsigned arm64 binary),
# not notarised: the first open is a right-click > Open, or
# `xattr -d com.apple.quarantine` on the app. Needs macOS: iconutil, hdiutil,
# codesign and sips are all system tools.
set -eu

BIN="${1:?binary}"
VERSION="${2:?version}"
OUT="${3:?output .dmg}"
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"

STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
APP="$STAGE/KMPLIFY Node.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$BIN" "$APP/Contents/MacOS/kmplify-node"
chmod 755 "$APP/Contents/MacOS/kmplify-node"
cat > "$APP/Contents/MacOS/KMPLIFY Node" <<'EOF'
#!/bin/sh
exec "$(dirname "$0")/kmplify-node" gui
EOF
chmod 755 "$APP/Contents/MacOS/KMPLIFY Node"
sed "s/__VERSION__/${VERSION#v}/g" "$ROOT/packaging/macos/Info.plist" > "$APP/Contents/Info.plist"

# The icon set from the one 1024 px master; every size Finder asks for.
ICONSET="$STAGE/kmplify-node.iconset"
mkdir -p "$ICONSET"
MASTER="$ROOT/packaging/icons/kmplify-node-1024.png"
for size in 16 32 128 256 512; do
  sips -z "$size" "$size" "$MASTER" --out "$ICONSET/icon_${size}x${size}.png" >/dev/null
  double=$((size * 2))
  sips -z "$double" "$double" "$MASTER" --out "$ICONSET/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/kmplify-node.icns"

codesign --force --deep --sign - "$APP"

# The usual drag-to-Applications layout.
DMGROOT="$STAGE/dmg"
mkdir -p "$DMGROOT"
cp -R "$APP" "$DMGROOT/"
ln -s /Applications "$DMGROOT/Applications"
rm -f "$OUT"
hdiutil create -volname "KMPLIFY Node" -srcfolder "$DMGROOT" -ov -format UDZO "$OUT" >/dev/null
echo "wrote $OUT"
