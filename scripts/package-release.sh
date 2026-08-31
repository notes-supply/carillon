#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
VERSION=${1:-0.0.0}
OUTPUT=${2:-"$ROOT/dist/carillon-$VERSION-linux-x86_64.tar.gz"}
STAGE="$ROOT/target/package/carillon-$VERSION"
PI_DIST=packages/runtime-pi/node_modules/@earendil-works/pi-coding-agent/dist
PI_PACKAGE=${PI_DIST%/dist}

rm -rf "$STAGE"
mkdir -p "$STAGE/bin/theme" "$STAGE/bin/assets" "$STAGE/licenses" "$STAGE/systemd" "$(dirname -- "$OUTPUT")"

cd "$ROOT"
cargo build --release --locked --bin carillon
bun build --compile packages/runtime-pi/src/index.ts --outfile "$STAGE/bin/carillon-runtime-pi"
bun build --compile --no-compile-autoload-bunfig \
  "$PI_DIST/bun/cli.js" "$PI_DIST/utils/image-resize-worker.js" \
  --outfile "$STAGE/bin/pi"
install -m 0755 target/release/carillon "$STAGE/bin/carillon"
install -m 0755 scripts/carillon-pi.sh "$STAGE/bin/carillon-pi"
install -m 0755 scripts/install-release.sh "$STAGE/install.sh"
install -m 0755 scripts/uninstall-release.sh "$STAGE/uninstall.sh"
install -m 0644 deploy/systemd/* "$STAGE/systemd/"
install -m 0644 "$PI_DIST"/modes/interactive/theme/*.json "$STAGE/bin/theme/"
install -m 0644 "$PI_DIST"/modes/interactive/assets/*.png "$STAGE/bin/assets/"
install -m 0644 "$PI_PACKAGE/package.json" "$PI_PACKAGE/README.md" "$PI_PACKAGE/CHANGELOG.md" "$STAGE/bin/"
cp -R "$PI_PACKAGE/docs" "$STAGE/bin/docs"
install -m 0644 licenses/pi-coding-agent.txt "$STAGE/licenses/"

tar -C "$(dirname -- "$STAGE")" -czf "$OUTPUT" "$(basename -- "$STAGE")"
printf '%s\n' "$OUTPUT"
