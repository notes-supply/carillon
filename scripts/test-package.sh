#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TEST_ROOT=$(mktemp -d)
trap 'rm -rf -- "$TEST_ROOT"' EXIT
trap 'exit 1' HUP INT TERM
FIXTURE=$TEST_ROOT/source
COMMANDS=$TEST_ROOT/commands
mkdir -p "$FIXTURE/scripts" "$COMMANDS"
cp "$ROOT/scripts/package-release.sh" "$FIXTURE/scripts/"
printf '[workspace.package]\nversion = "1.2.3"\n' >"$FIXTURE/Cargo.toml"
# Only the toolchain is substituted. Real staging, copying and tar publication
# prove that the selected binary path and archive label agree with both targets.
cp "$ROOT/scripts/carillon-pi.sh" "$ROOT/scripts/register-services.sh" \
  "$ROOT/scripts/install-release.sh" "$ROOT/scripts/uninstall-release.sh" \
  "$ROOT/scripts/npm-notices.mjs" "$FIXTURE/scripts/"
mkdir -p "$FIXTURE/deploy/systemd" "$FIXTURE/licenses" "$FIXTURE/packages/discord-pi/skill"
cp "$ROOT/LICENSE" "$FIXTURE/"
cp "$ROOT/licenses/MIT.txt" "$ROOT/licenses/bun.md" "$FIXTURE/licenses/"
# One npm dependency with a licence file, one without, as pnpm lists them.
mkdir -p "$FIXTURE/npm/with-file" "$FIXTURE/npm/without-file"
printf 'fixture licence text\n' >"$FIXTURE/npm/with-file/LICENSE"
printf '{"MIT":[{"name":"with-file","versions":["1.0.0"],"paths":["%s"],"license":"MIT"},{"name":"without-file","versions":["2.0.0"],"paths":["%s"],"license":"MIT","author":"Someone"}]}\n' \
  "$FIXTURE/npm/with-file" "$FIXTURE/npm/without-file" >"$FIXTURE/npm/listing.json"
cp "$ROOT/packages/discord-pi/skill/SKILL.md" "$FIXTURE/packages/discord-pi/skill/SKILL.md"
printf unit >"$FIXTURE/deploy/systemd/example.service"
printf license >"$FIXTURE/licenses/pi-coding-agent.txt"
PI=$FIXTURE/packages/runtime-pi/node_modules/@earendil-works/pi-coding-agent
mkdir -p "$PI/dist/modes/interactive/theme" "$PI/dist/modes/interactive/assets" "$PI/docs"
for path in package.json README.md CHANGELOG.md dist/modes/interactive/theme/theme.json dist/modes/interactive/assets/image.png docs/README.md; do
  printf fixture >"$PI/$path"
done
# Pi's image library, resolved from Pi's own dependencies.
PHOTON=$PI/node_modules/@silvia-odwyer/photon-node
mkdir -p "$PHOTON"
printf '{"name":"@silvia-odwyer/photon-node","main":"photon_rs.js"}\n' >"$PHOTON/package.json"
printf photon >"$PHOTON/photon_rs_bg.wasm"
REAL_BUN=$(command -v bun)
export REAL_BUN
cat >"$COMMANDS/toolchain" <<'COMMAND'
#!/bin/sh
set -eu
case "${0##*/}" in
  rustc) [ "$*" = '--print host-tuple' ]; printf '%s\n' "$TEST_TARGET" ;;
  pnpm) [ "$1 $2" = 'licenses list' ]; cat npm/listing.json ;;
  cargo)
    if [ "$1" = about ]; then
      while [ "$1" != -o ]; do shift; done
      printf 'rust notices\n' >"$2"
      exit 0
    fi
    printf '%s\n' "$*" >>"$TRACE.cargo"
    target= profile=release package=
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --target) target=$2; shift ;;
        --profile) profile=$2; shift ;;
        -p) package=$2; shift ;;
      esac
      shift
    done
    if [ "$target" = wasm32-unknown-unknown ]; then
      [ "$profile" = wasm ]
      mkdir -p "target/$target/$profile"
      printf '%s\n' "$package" >"target/$target/$profile/$(printf '%s' "$package" | tr - _).wasm"
    else
      [ "$target" = "$TEST_TARGET" ]
      mkdir -p "target/$target/release"
      for binary in carillon carillon-discord; do
        printf '%s\n' "$target" >"target/$target/release/$binary"
      done
    fi
    ;;
  bun)
    [ "$1" != -e ] || exec "$REAL_BUN" "$@"
    printf '%s\n' "$*" >>"$TRACE.bun"
    target= output=
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --target) target=$2; shift ;;
        --target=*) target=${1#--target=} ;;
        --outfile) output=$2; shift ;;
      esac
      shift
    done
    [ "$target" = "$TEST_BUN_TARGET" ] || [ "$target" = bun ]
    printf '%s\n' "$target" >"$output"
    ;;
  *) exit 2 ;;
esac
COMMAND
chmod 0755 "$COMMANDS/toolchain"
for command in rustc cargo bun pnpm; do ln -s toolchain "$COMMANDS/$command"; done
for arch in x86_64 aarch64; do
  bun_arch=x64
  [ "$arch" != aarch64 ] || bun_arch=arm64
  target=$arch-unknown-linux-gnu
  archive=$(env PATH="$COMMANDS:$PATH" TEST_TARGET="$target" \
    TEST_BUN_TARGET="bun-linux-$bun_arch" TRACE="$TEST_ROOT/$arch" \
    CARGO_BUILD_TARGET=conflicting-target "$FIXTURE/scripts/package-release.sh")
  [ "$archive" = "$FIXTURE/dist/carillon-1.2.3-linux-$arch.tar.gz" ]
  for binary in carillon carillon-discord; do
    [ "$(tar -xOf "$archive" "carillon-1.2.3/bin/$binary")" = "$target" ]
  done
  for binary in carillon-runtime-pi carillon-runtime-t3; do
    [ "$(tar -xOf "$archive" "carillon-1.2.3/bin/$binary")" = "bun-linux-$bun_arch" ]
  done
  # Pi's command line is a mode of the runtime executable, not a second one.
  if tar -tzf "$archive" carillon-1.2.3/bin/pi >/dev/null 2>&1; then
    echo 'the release ships a second Pi executable' >&2
    exit 1
  fi
  [ "$(tar -xOf "$archive" carillon-1.2.3/lib/carillon_discord_module.wasm)" = carillon-discord-module ]
  [ "$(tar -xOf "$archive" carillon-1.2.3/lib/examples/card-colours.wasm)" = carillon-card-colours ]
  # Carillon's licence and every notice ship in licenses/, which installation keeps.
  tar -xOf "$archive" carillon-1.2.3/licenses/Apache-2.0.txt | cmp - "$ROOT/LICENSE"
  [ "$(tar -xOf "$archive" carillon-1.2.3/licenses/rust.txt)" = 'rust notices' ]
  tar -xOf "$archive" carillon-1.2.3/licenses/npm.txt >"$TEST_ROOT/$arch.npm"
  grep -Fx 'fixture licence text' "$TEST_ROOT/$arch.npm" >/dev/null
  grep -F 'without-file 2.0.0 (MIT)' "$TEST_ROOT/$arch.npm" >/dev/null
  grep -F 'declares MIT, by Someone; the standard text is licenses/MIT.txt' "$TEST_ROOT/$arch.npm" >/dev/null
  [ "$(tar -xOf "$archive" carillon-1.2.3/bin/photon_rs_bg.wasm)" = photon ]
  # The manifest lists exactly the shipped files, marking what must be executable.
  tar -xOf "$archive" carillon-1.2.3/MANIFEST >"$TEST_ROOT/$arch.manifest"
  for line in 'x install.sh' 'x bin/carillon' 'x bin/carillon-discord' 'x bin/carillon-register-services' \
    'r lib/carillon_discord_module.wasm' 'r lib/examples/card-colours.wasm' \
    'r lib/carillon-discord-pi.js' 'r lib/carillon-discord-SKILL.md' \
    'r bin/photon_rs_bg.wasm' 'r systemd/example.service' 'r licenses/Apache-2.0.txt' \
    'r licenses/MIT.txt' 'r licenses/bun.md' 'r licenses/pi-coding-agent.txt' \
    'r licenses/rust.txt' 'r licenses/npm.txt'; do
    grep -Fx "$line" "$TEST_ROOT/$arch.manifest" >/dev/null
  done
  [ "$(wc -l <"$TEST_ROOT/$arch.manifest")" -eq \
    "$(tar -tzvf "$archive" | grep -c '^-' | awk '{ print $1 - 1 }')" ]
  [ "$(wc -l <"$TEST_ROOT/$arch.cargo")" -eq 3 ]
  [ "$(wc -l <"$TEST_ROOT/$arch.bun")" -eq 3 ]
  # No compiled provider loads a bunfig.toml from where it is started.
  [ "$(grep -c -- '--compile ' "$TEST_ROOT/$arch.bun")" -eq 2 ]
  if grep -- '--compile ' "$TEST_ROOT/$arch.bun" | grep -v -- '--no-compile-autoload-bunfig'; then
    echo 'a compiled provider may load bunfig.toml' >&2
    exit 1
  fi
  printf 'package target selection passed: %s\n' "$arch"
done
rm -rf "$FIXTURE/target" "$FIXTURE/dist"
if env PATH="$COMMANDS:$PATH" TEST_TARGET=x86_64-apple-darwin \
  TRACE="$TEST_ROOT/unsupported" "$FIXTURE/scripts/package-release.sh" >"$TEST_ROOT/refused.log" 2>&1; then
  echo 'packager accepted an unsupported release host' >&2
  exit 1
fi
grep -F 'unsupported release build host' "$TEST_ROOT/refused.log" >/dev/null
[ ! -e "$FIXTURE/target" ] && [ ! -e "$FIXTURE/dist" ]
[ ! -e "$TEST_ROOT/unsupported.cargo" ] && [ ! -e "$TEST_ROOT/unsupported.bun" ]
printf '%s\n' 'package target refusal passed before staging/build'
