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
BUNDLE="$ROOT/target/pi-bundle"

rm -rf "$STAGE"
mkdir -p "$STAGE/bin/theme" "$STAGE/bin/assets" "$STAGE/lib/examples" "$STAGE/licenses" \
  "$STAGE/systemd" "$(dirname -- "$OUTPUT")"

# One executable serves the runtime and, as `carillon-runtime-pi pi …`, Pi's own
# command line. Each mode is imported only once chosen: Pi's Bun entry, which
# restores a sandboxed environment and runs Pi's runtime setup before its CLI,
# or that runtime setup (Bun OAuth flows, Bedrock, embedded QuickJS) before the
# runtime's CLI. Pi reads its arguments without the mode's.
rm -rf "$BUNDLE"
mkdir -p "$BUNDLE/src/extensions/codemode" "$BUNDLE/src/utils"
cat >"$BUNDLE/carillon-runtime-pi.ts" <<ENTRY
if (process.argv[2] === "pi") {
  process.argv.splice(2, 1);
  await import("$ROOT/$PI_DIST/bun/cli.js");
} else {
  await import("$ROOT/$PI_DIST/bun/runtime-setup.js");
  await import("$ROOT/packages/runtime-pi/src/cli.ts");
}
ENTRY
# Pi's worker specifiers use its source layout, so keep those entrypoint
# identities even when building from npm's dist.
printf 'import "%s";\n' "$ROOT/$PI_DIST/extensions/codemode/worker.js" \
  >"$BUNDLE/src/extensions/codemode/worker.ts"
printf 'import "%s";\n' "$ROOT/$PI_DIST/utils/image-resize-worker.js" \
  >"$BUNDLE/src/utils/image-resize-worker.ts"

bun build --target=bun packages/discord-pi/src/index.ts --outfile "$STAGE/lib/carillon-discord-pi.js"
install -m 0644 packages/discord-pi/skill/SKILL.md "$STAGE/lib/carillon-discord-SKILL.md"

cargo build --release --locked --target "$TARGET" --target-dir "$ROOT/target" --bin carillon --bin carillon-discord
# The Discord modules run in the provider's Wasm sandbox; they are data, not
# native binaries, and are built for the wasm32 target regardless of host.
# The example custom module ships built, to try without a toolchain; its
# source is examples/card-colours.
cargo build --locked --profile wasm --target wasm32-unknown-unknown --target-dir "$ROOT/target" \
  -p carillon-discord-bundled -p carillon-card-colours
# Compiled providers never load a bunfig.toml from where they are started.
bun build --compile --target "$BUN_TARGET" --no-compile-autoload-bunfig \
  --root "$BUNDLE" "$BUNDLE/carillon-runtime-pi.ts" \
  "$BUNDLE/src/extensions/codemode/worker.ts" "$BUNDLE/src/utils/image-resize-worker.ts" \
  --outfile "$STAGE/bin/carillon-runtime-pi"
# The T3 runtime is an optional provider: shipped as its own executable and
# unit, registered and started only when the operator configures it.
bun build --compile --target "$BUN_TARGET" --no-compile-autoload-bunfig \
  packages/runtime-t3/src/cli.ts --outfile "$STAGE/bin/carillon-runtime-t3"
install -m 0755 "target/$TARGET/release/carillon" "$STAGE/bin/carillon"
install -m 0755 "target/$TARGET/release/carillon-discord" "$STAGE/bin/carillon-discord"
install -m 0644 target/wasm32-unknown-unknown/wasm/carillon_discord_bundled.wasm "$STAGE/lib/carillon_discord_module.wasm"
install -m 0644 target/wasm32-unknown-unknown/wasm/carillon_card_colours.wasm "$STAGE/lib/examples/card-colours.wasm"
install -m 0755 scripts/carillon-pi.sh "$STAGE/bin/carillon-pi"
install -m 0755 scripts/register-services.sh "$STAGE/bin/carillon-register-services"
install -m 0755 scripts/install-release.sh "$STAGE/install.sh"
install -m 0755 scripts/uninstall-release.sh "$STAGE/uninstall.sh"
install -m 0644 deploy/systemd/* "$STAGE/systemd/"
install -m 0644 "$PI_DIST"/modes/interactive/theme/*.json "$STAGE/bin/theme/"
install -m 0644 "$PI_DIST"/modes/interactive/assets/*.png "$STAGE/bin/assets/"
install -m 0644 "$PI_PACKAGE/package.json" "$PI_PACKAGE/README.md" "$PI_PACKAGE/CHANGELOG.md" "$STAGE/bin/"
cp -R "$PI_PACKAGE/docs" "$STAGE/bin/docs"
# Pi normalizes images with Photon. A compiled executable cannot read the
# build tree, so Photon's loader looks for its WebAssembly beside the
# executable; without it Pi omits every image.
PHOTON=$(node -p 'require("path").dirname(require.resolve("@silvia-odwyer/photon-node/package.json", {paths: [require("fs").realpathSync(process.argv[1])]}))' "$PI_PACKAGE")
install -m 0644 "$PHOTON/photon_rs_bg.wasm" "$STAGE/bin/"
# Licences travel in licenses/, which installation keeps in app/licenses:
# Carillon's own (Apache-2.0, also the standard text for packages that
# publish none), the standard MIT text (SPDX), Bun's (oven-sh/bun LICENSE.md
# at bun-v1.4.4), Pi's, and notices for the Rust crates and npm packages.
install -m 0644 LICENSE "$STAGE/licenses/Apache-2.0.txt"
install -m 0644 licenses/pi-coding-agent.txt licenses/bun.md licenses/MIT.txt "$STAGE/licenses/"
cargo about generate --locked --fail -c licenses/about.toml licenses/rust.hbs -o "$STAGE/licenses/rust.txt"
node scripts/npm-notices.mjs "$STAGE" >"$STAGE/licenses/npm.txt"

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
