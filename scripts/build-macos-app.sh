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
[[ -f "$app_pkg/Resources/AppIcon.icns" ]] && cp "$app_pkg/Resources/AppIcon.icns" "$out/Contents/Resources/"
codesign --force --sign - "$out"

echo "built $out"
