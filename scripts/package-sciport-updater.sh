#!/usr/bin/env bash
# Called only after final codesigning and DMG creation. Never sign an archive of
# an earlier app bundle: updater and manual installer must deliver identical bytes.
set -euo pipefail
: "${RUNNER_TEMP:?}"
: "${RELEASE_TAG:?}"
: "${SCIPORT_UPDATER_PUBLIC_KEY:?Configure the stable public key first}"
: "${TAURI_SIGNING_PRIVATE_KEY:?Configure the signing secret first}"
[[ "$RELEASE_TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]]
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="${RELEASE_TAG#v}"
OUTPUT="$RUNNER_TEMP/sciport-release"
BUNDLE="$ROOT/desktop/src-tauri/target/aarch64-apple-darwin/release/bundle/macos"
ARCHIVE="SciPort_${VERSION}_aarch64.app.tar.gz"
[[ -f "$OUTPUT/RELEASE_NOTES.md" && -f "$OUTPUT/SHA256SUMS" ]]
[[ ! -e "$OUTPUT/$ARCHIVE" && ! -e "$OUTPUT/latest.json" ]]
codesign --verify --deep --strict "$BUNDLE/SciPort.app"
COPYFILE_DISABLE=1 tar -czf "$OUTPUT/$ARCHIVE" -C "$BUNDLE" SciPort.app
# The CLI consumes the secret from its environment; never put it in argv or logs.
"$ROOT/desktop/node_modules/.bin/tauri" signer sign "$OUTPUT/$ARCHIVE" >/dev/null
# Verify the signature against the same public key embedded in the app. A
# mismatched repository variable/secret must fail before public release upload.
env -u TAURI_SIGNING_PRIVATE_KEY -u TAURI_SIGNING_PRIVATE_KEY_PASSWORD cargo run --quiet --locked --release --target aarch64-apple-darwin \
  --manifest-path "$ROOT/desktop/src-tauri/Cargo.toml" --example verify_app_update -- "$OUTPUT/$ARCHIVE"
node "$ROOT/scripts/write-app-update-manifest.mjs" "$VERSION" "$OUTPUT/$ARCHIVE.sig" \
  "$OUTPUT/RELEASE_NOTES.md" "$OUTPUT/latest.json"
(cd "$OUTPUT" && shasum -a 256 "$ARCHIVE" "$ARCHIVE.sig" latest.json >> SHA256SUMS)
