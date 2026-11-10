#!/bin/sh
# Build OpenSSC.app — a macOS bundle around the openssc Screen Sharing client.
#
#   ./packaging/mac/build_openssc.sh            build from source (cargo release)
#   ./packaging/mac/build_openssc.sh --no-build reuse the current release binary
#
# The bundle is written to packaging/mac/OpenSSC.app. The binary links only
# system frameworks (otool -L shows nothing outside /System and /usr/lib), so
# the bundle needs just the binary, an Info.plist and an icon. The icon is
# rendered from the repo's packaging/openssc.svg with rsvg-convert + iconutil;
# without rsvg-convert (or iconutil) the bundle ships without a custom icon.
#
# The app is ad-hoc signed so it launches locally and keeps a stable identity
# for the Accessibility/Input Monitoring prompts the keyboard grab needs on
# first run. Keep the bundle in one place (e.g. drag it to /Applications) so
# macOS remembers the granted permissions across rebuilds. Any build flags the
# binary supports can be passed through Finder later via
#   open OpenSSC.app --args --host mm.local --mvs
set -e

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

if [ "$1" != "--no-build" ]; then
    cargo build --release -p openssc
fi

version=$(awk '/^\[workspace\.package\]/{f=1} f && /^version *=/{gsub("[^0-9.]","",$3); print $3; exit}' Cargo.toml)
app="packaging/mac/OpenSSC.app"

rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp target/release/openssc "$app/Contents/MacOS/OpenSSC"

icon="$app/Contents/Resources/OpenSSC.icns"
if command -v rsvg-convert >/dev/null 2>&1 && command -v iconutil >/dev/null 2>&1; then
    set_dir=$(mktemp -d)/OpenSSC.iconset
    mkdir -p "$set_dir"
    for s in 16 32 64 128 256 512 1024; do
        rsvg-convert -w "$s" -h "$s" packaging/openssc.svg -o "$set_dir/icon_${s}x${s}.png" 2>/dev/null || true
    done
    # retina pairs (@2x) rendered from the same vector source
    mv "$set_dir/icon_32x32.png"    "$set_dir/icon_16x16@2x.png"    2>/dev/null || true
    mv "$set_dir/icon_64x64.png"    "$set_dir/icon_32x32@2x.png"    2>/dev/null || true
    mv "$set_dir/icon_256x256.png"  "$set_dir/icon_128x128@2x.png"  2>/dev/null || true
    mv "$set_dir/icon_1024x1024.png" "$set_dir/icon_512x512@2x.png" 2>/dev/null || true
    cp "$set_dir/icon_16x16@2x.png" "$set_dir/icon_32x32.png"
    cp "$set_dir/icon_32x32@2x.png" "$set_dir/icon_128x128.png"
    cp "$set_dir/icon_128x128@2x.png" "$set_dir/icon_512x512.png"
    rsvg-convert -w 16 -h 16 packaging/openssc.svg -o "$set_dir/icon_16x16.png" 2>/dev/null || true
    if iconutil -c icns "$set_dir" -o "$icon" 2>/dev/null; then
        echo "icon: rendered from packaging/openssc.svg"
    else
        rm -f "$icon"
        echo "icon: iconutil failed; bundling without a custom icon"
    fi
fi
[ -f "$icon" ] || rm -f "$icon"

cat > "$app/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key>                <string>OpenSSC</string>
    <key>CFBundleDisplayName</key>          <string>OpenSSC</string>
    <key>CFBundleExecutable</key>           <string>OpenSSC</string>
    <key>CFBundleIdentifier</key>           <string>io.openssc.OpenSSC</string>
    <key>CFBundleVersion</key>              <string>$version</string>
    <key>CFBundleShortVersionString</key>   <string>$version</string>
    <key>CFBundlePackageType</key>          <string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key> <string>6.0</string>
    <key>CFBundleIconFile</key>             <string>OpenSSC</string>
    <key>NSHighResolutionCapable</key>      <true/>
    <key>LSMinimumSystemVersion</key>       <string>10.15</string>
    <key>LSApplicationCategoryType</key>    <string>public.app-category.utilities</string>
    <key>NSHumanReadableCopyright</key>     <string>Protocol-compatible Apple Screen Sharing client. Connects with the remote host's SSH account credentials; defaults are mm.local / tai.</string>
</dict>
</plist>
PLIST

codesign --force --sign - "$app" 2>/dev/null || echo "codesign: ad-hoc signing failed; the app still launches locally"

echo "built: $app (openssc $version)"
