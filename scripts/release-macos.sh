#!/usr/bin/env bash
# Builds the app signed with Developer ID, notarizes, staples, and zips it.
#
# One-time setup: a Developer ID Application certificate for the team in the
# keychain, and notarization credentials stored under $KEYCHAIN_PROFILE:
#   xcrun notarytool store-credentials "$KEYCHAIN_PROFILE" \
#     --apple-id <apple-id> --team-id "$TEAM_ID" --password <app-specific-password>
set -euo pipefail

TEAM_ID="${TEAM_ID:-Z74U5327X4}"
KEYCHAIN_PROFILE="${KEYCHAIN_PROFILE:-rekord-ripper-notary}"

root="$(cd "$(dirname "$0")/.." && pwd)"
app="$root/target/macos/Rekord Ripper.app"
zip="$root/target/macos/RekordRipper.zip"

echo "==> Building"
"$root/scripts/build-macos-app.sh"
# The build falls back to ad hoc signing, which notarization rejects.
if ! codesign -dv "$app" 2>&1 | grep -q "TeamIdentifier=$TEAM_ID"; then
  echo "error: $app is not signed by team $TEAM_ID" >&2
  exit 1
fi

echo "==> Notarizing"
rm -f "$zip"
ditto -c -k --keepParent "$app" "$zip"
xcrun notarytool submit "$zip" --keychain-profile "$KEYCHAIN_PROFILE" --wait

echo "==> Stapling"
xcrun stapler staple "$app"
xcrun stapler validate "$app"
# Re-zip so the distributed copy carries the ticket.
rm "$zip"
ditto -c -k --keepParent "$app" "$zip"

echo "==> Verifying"
codesign --verify --deep --strict --verbose=2 "$app"
spctl --assess --type execute --verbose=2 "$app"

echo "==> Done: $zip"
