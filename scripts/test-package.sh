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
  "$ROOT/scripts/install-release.sh" "$ROOT/scripts/uninstall-release.sh" "$FIXTURE/scripts/"
mkdir -p "$FIXTURE/deploy/systemd" "$FIXTURE/licenses"
printf unit >"$FIXTURE/deploy/systemd/example.service"
printf license >"$FIXTURE/licenses/pi-coding-agent.txt"
PI=$FIXTURE/packages/runtime-pi/node_modules/@earendil-works/pi-coding-agent
mkdir -p "$PI/dist/modes/interactive/theme" "$PI/dist/modes/interactive/assets" "$PI/docs"
for path in package.json README.md CHANGELOG.md dist/modes/interactive/theme/theme.json dist/modes/interactive/assets/image.png docs/README.md; do
  printf fixture >"$PI/$path"
done
REAL_BUN=$(command -v bun)
export REAL_BUN
cat >"$COMMANDS/toolchain" <<'COMMAND'
#!/bin/sh
set -eu
case "${0##*/}" in
  rustc) [ "$*" = '--print host-tuple' ]; printf '%s\n' "$TEST_TARGET" ;;
  cargo)
    printf '%s\n' "$*" >>"$TRACE.cargo"
    target= profile=release
    while [ "$#" -gt 0 ]; do
      case "$1" in
        --target) target=$2; shift ;;
        --profile) profile=$2; shift ;;
      esac
      shift
    done
    if [ "$target" = wasm32-unknown-unknown ]; then
      [ "$profile" = wasm ]
      mkdir -p "target/$target/$profile"
      printf 'wasm\n' >"target/$target/$profile/carillon_discord_commands.wasm"
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
        --outfile) output=$2; shift ;;
      esac
      shift
    done
    [ "$target" = "$TEST_BUN_TARGET" ]
    printf '%s\n' "$target" >"$output"
    ;;
  *) exit 2 ;;
esac
COMMAND
chmod 0755 "$COMMANDS/toolchain"
for command in rustc cargo bun; do ln -s toolchain "$COMMANDS/$command"; done
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
  for binary in carillon-runtime-pi pi; do
    [ "$(tar -xOf "$archive" "carillon-1.2.3/bin/$binary")" = "bun-linux-$bun_arch" ]
  done
  [ "$(tar -xOf "$archive" carillon-1.2.3/lib/carillon_discord_commands.wasm)" = wasm ]
  # The manifest lists exactly the shipped files, marking what must be executable.
  tar -xOf "$archive" carillon-1.2.3/MANIFEST >"$TEST_ROOT/$arch.manifest"
  for line in 'x install.sh' 'x bin/carillon' 'x bin/carillon-discord' 'x bin/carillon-register-services' \
    'r lib/carillon_discord_commands.wasm' 'r systemd/example.service'; do
    grep -Fx "$line" "$TEST_ROOT/$arch.manifest" >/dev/null
  done
  [ "$(wc -l <"$TEST_ROOT/$arch.manifest")" -eq \
    "$(tar -tzvf "$archive" | grep -c '^-' | awk '{ print $1 - 1 }')" ]
  [ "$(wc -l <"$TEST_ROOT/$arch.cargo")" -eq 2 ]
  [ "$(wc -l <"$TEST_ROOT/$arch.bun")" -eq 2 ]
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
