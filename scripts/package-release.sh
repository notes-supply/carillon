#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
VERSION=${1:-0.0.0}
OUTPUT=${2:-"$ROOT/dist/carillon-$VERSION-linux-x86_64.tar.gz"}
STAGE="$ROOT/target/package/carillon-$VERSION"

rm -rf "$STAGE"
mkdir -p "$STAGE/bin" "$STAGE/systemd" "$(dirname -- "$OUTPUT")"

cd "$ROOT"
cargo build --release --locked --bin carillon
bun build --compile packages/runtime-pi/src/index.ts --outfile "$STAGE/bin/carillon-runtime-pi"
install -m 0755 target/release/carillon "$STAGE/bin/carillon"
install -m 0755 scripts/install-release.sh "$STAGE/install.sh"
install -m 0644 deploy/systemd/*.service "$STAGE/systemd/"

tar -C "$(dirname -- "$STAGE")" -czf "$OUTPUT" "$(basename -- "$STAGE")"
printf '%s\n' "$OUTPUT"
