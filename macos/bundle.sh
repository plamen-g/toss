#!/bin/sh
# Wraps a built toss in toss.app, so macOS lists it in Applications and Launchpad and shows its icon in
# the Dock, and puts that in toss-macos.dmg: open it and drag toss onto the Applications folder beside it.
# Run it on a Mac, from the repository: sh macos/bundle.sh [binary] [folder]
# (by default target/release/toss, into target/release).
set -eu
bin=${1:-target/release/toss}
out=${2:-target/release}
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n 1)
app="$out/toss.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$bin" "$app/Contents/MacOS/toss"

# The icon at every size macOS asks for, from one 1024 px picture.
icons="$(mktemp -d)/toss.iconset"
mkdir -p "$icons"
for size in 16 32 128 256 512; do
    sips -z "$size" "$size" assets/icon-mac.png --out "$icons/icon_${size}x${size}.png" >/dev/null
    sips -z "$((size * 2))" "$((size * 2))" assets/icon-mac.png --out "$icons/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil -c icns "$icons" -o "$app/Contents/Resources/toss.icns"

cat > "$app/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleName</key><string>toss</string>
  <key>CFBundleDisplayName</key><string>toss</string>
  <key>CFBundleIdentifier</key><string>com.github.plamen-g.toss</string>
  <key>CFBundleExecutable</key><string>toss</string>
  <key>CFBundleIconFile</key><string>toss</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>$version</string>
  <key>CFBundleVersion</key><string>$version</string>
  <key>LSMinimumSystemVersion</key><string>11.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>NSLocalNetworkUsageDescription</key><string>toss finds your devices on this network, and serves the phone page to your phone.</string>
  <key>NSDownloadsFolderUsageDescription</key><string>toss saves the files you receive in Downloads.</string>
  <key>NSDocumentsFolderUsageDescription</key><string>toss sends the files you choose.</string>
  <key>NSDesktopFolderUsageDescription</key><string>toss sends the files you choose.</string>
</dict></plist>
EOF

# Apple silicon runs only signed code. Ad hoc, since toss isn't signed with an Apple developer ID yet.
codesign --force --sign - "$app"

# The disk image: the app, and a shortcut to Applications to drag it onto.
stage=$(mktemp -d)
ditto "$app" "$stage/toss.app"
ln -s /Applications "$stage/Applications"
hdiutil create -volname toss -srcfolder "$stage" -ov -format UDZO "$out/toss-macos.dmg" >/dev/null
echo "$app"
echo "$out/toss-macos.dmg"
