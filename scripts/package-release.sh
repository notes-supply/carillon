#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
# Cargo embeds this identity in the core and Discord binaries. A supplied version
# is an assertion, not an independent label for differently versioned artifacts.
VERSION=$(bun -e 'import manifest from "./crates/carillon-core/Cargo.toml"; console.log(manifest.package.version)')
[ "${1:-$VERSION}" = "$VERSION" ] || {
  echo "requested release version $1 differs from the Carillon crate version $VERSION" >&2
  exit 1
}
# Both toolchains build for the same explicit native target. In particular, a
# Cargo environment/config cross target must not silently select another CPU.
TARGET=$(rustc --print host-tuple)
case "$TARGET" in
  x86_64-unknown-linux-gnu) ARCH=x86_64; BUN_TARGET=bun-linux-x64 ;;
  aarch64-unknown-linux-gnu) ARCH=aarch64; BUN_TARGET=bun-linux-arm64 ;;
  *) echo "unsupported release build host: $TARGET" >&2; exit 1 ;;
esac
OUTPUT=${2:-"$ROOT/dist/carillon-$VERSION-linux-$ARCH.tar.gz"}
STAGE="$ROOT/target/package/carillon-$VERSION"
PI_DIST=packages/runtime-pi/node_modules/@earendil-works/pi-coding-agent/dist
PI_PACKAGE=${PI_DIST%/dist}

rm -rf "$STAGE"
mkdir -p "$STAGE/bin/theme" "$STAGE/bin/assets" "$STAGE/licenses" "$STAGE/systemd" "$(dirname -- "$OUTPUT")"

cargo build --release --locked --target "$TARGET" --target-dir "$ROOT/target" --bin carillon
bun build --compile --target "$BUN_TARGET" packages/runtime-pi/src/index.ts --outfile "$STAGE/bin/carillon-runtime-pi"
bun build --compile --target "$BUN_TARGET" --no-compile-autoload-bunfig \
  "$PI_DIST/bun/cli.js" "$PI_DIST/utils/image-resize-worker.js" \
  --outfile "$STAGE/bin/pi"
install -m 0755 "target/$TARGET/release/carillon" "$STAGE/bin/carillon"
install -m 0755 scripts/carillon-pi.sh "$STAGE/bin/carillon-pi"
install -m 0755 scripts/register-services.sh "$STAGE/bin/carillon-register-services"
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
