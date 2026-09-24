#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"
# Cargo embeds this identity in the core and Discord binaries. A supplied version
# is an assertion, not an independent label for differently versioned artifacts.
VERSION=$(bun -e 'import manifest from "./Cargo.toml"; console.log(manifest.workspace.package.version)')
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
mkdir -p "$STAGE/bin/theme" "$STAGE/bin/assets" "$STAGE/lib" "$STAGE/licenses" "$STAGE/systemd" "$(dirname -- "$OUTPUT")"

cargo build --release --locked --target "$TARGET" --target-dir "$ROOT/target" --bin carillon --bin carillon-discord
# The Discord command module runs in the provider's Wasm sandbox; it is data,
# not a native binary, and is built for the wasm32 target regardless of host.
cargo build --locked --profile wasm --target wasm32-unknown-unknown --target-dir "$ROOT/target" -p carillon-discord-commands
bun build --compile --target "$BUN_TARGET" packages/runtime-pi/src/cli.ts --outfile "$STAGE/bin/carillon-runtime-pi"
bun build --compile --target "$BUN_TARGET" --no-compile-autoload-bunfig \
  "$PI_DIST/bun/cli.js" "$PI_DIST/utils/image-resize-worker.js" \
  --outfile "$STAGE/bin/pi"
install -m 0755 "target/$TARGET/release/carillon" "$STAGE/bin/carillon"
install -m 0755 "target/$TARGET/release/carillon-discord" "$STAGE/bin/carillon-discord"
install -m 0644 target/wasm32-unknown-unknown/wasm/carillon_discord_commands.wasm "$STAGE/lib/carillon_discord_commands.wasm"
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

# Installation checks the payload against this list before changing anything:
# every shipped file, marked `x` when the installer must be able to execute it.
if [ -n "$(cd "$STAGE" && find . ! -type f ! -type d -o -name '*
*')" ]; then
  echo "release staging contains links, special files or newlines in names" >&2
  exit 1
fi
(cd "$STAGE" && find . -type f -printf '%P\n' | LC_ALL=C sort | while IFS= read -r path; do
  if [ -x "$path" ]; then kind=x; else kind=r; fi
  printf '%s %s\n' "$kind" "$path"
done) >"$STAGE.manifest"
mv -- "$STAGE.manifest" "$STAGE/MANIFEST"
tar -C "$(dirname -- "$STAGE")" -czf "$OUTPUT" "$(basename -- "$STAGE")"
printf '%s\n' "$OUTPUT"
