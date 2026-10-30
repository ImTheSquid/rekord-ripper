#!/usr/bin/env bash
# Builds target/macos/RekordRipper.app: Rust staticlib → SwiftPM → bundle.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
app_pkg="$root/macos/app"
out="$root/target/macos/RekordRipper.app"

# C dependencies otherwise target the SDK's macOS, newer than the app's minimum.
# Not MACOSX_DEPLOYMENT_TARGET: that also reaches proc-macro dylibs, and rustc
# then fails to load them. Its own target dir, because changed CFLAGS rebuild
# the C dependencies and would invalidate the CLI's release cache.
export CFLAGS="-mmacosx-version-min=14.0"
rust_target="$root/target/macos-rust"
cargo build --manifest-path "$root/Cargo.toml" -p rekord-ripper-macos --release --target-dir "$rust_target"
mkdir -p "$app_pkg/.rust"
cp "$rust_target/release/librekord_ripper_macos.a" "$app_pkg/.rust/"

swift build --package-path "$app_pkg" -c release --product RekordRipper
bin="$(swift build --package-path "$app_pkg" -c release --show-bin-path)/RekordRipper"

rm -rf "$out"
mkdir -p "$out/Contents/MacOS" "$out/Contents/Resources"
cp "$bin" "$out/Contents/MacOS/RekordRipper"
cp "$app_pkg/Resources/Info.plist" "$out/Contents/Info.plist"
# The Icon Composer icon needs Xcode's actool; the .icns made from it stands in without.
if xcrun --find actool >/dev/null 2>&1; then
  xcrun actool "$app_pkg/Resources/AppIcon.icon" --compile "$out/Contents/Resources" \
    --platform macosx --minimum-deployment-target 14.0 --app-icon AppIcon \
    --output-partial-info-plist "$(mktemp -d)/icon.plist" --errors >/dev/null
else
  cp "$app_pkg/Resources/AppIcon.icns" "$out/Contents/Resources/"
fi
cp "$app_pkg/Resources/Credits.html" "$out/Contents/Resources/"

# Alluja LLC's Developer ID when it is in the keychain, otherwise ad hoc.
identity="${CODESIGN_IDENTITY:-Developer ID Application: Alluja LLC (Z74U5327X4)}"
if security find-identity -v -p codesigning | grep -qF "$identity"; then
  codesign --force --options runtime --timestamp --sign "$identity" "$out"
else
  codesign --force --sign - "$out"
fi

echo "built $out"
