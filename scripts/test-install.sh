#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
VERSION=$(cd "$ROOT" && bun -e 'import manifest from "./Cargo.toml"; console.log(manifest.workspace.package.version)')
RELEASE=${1:-"$ROOT/target/package/carillon-$VERSION"}
BASE_PORT=${CARILLON_TEST_BASE_PORT:-44800}

# This tests the bundled Discord/Pi release, not a limit on provider extensibility.
# Reinstallation below replaces binaries with the same schema; it is not a
# database/configuration migration or a promise to support old beta formats.

for artifact in \
  bin/carillon \
  bin/carillon-discord \
  lib/carillon_discord_commands.wasm \
  bin/carillon-runtime-pi \
  bin/pi \
  bin/carillon-pi \
  bin/carillon-register-services \
  install.sh \
  uninstall.sh \
  MANIFEST \
  systemd/carillon.target \
  systemd/carillon-core.service \
  systemd/carillon-discord.service \
  systemd/carillon-runtime-pi.service
do
  [ -e "$RELEASE/$artifact" ] || { printf 'release artifact missing: %s\n' "$artifact" >&2; exit 1; }
done
[ "$(find "$RELEASE/systemd" -maxdepth 1 -type f | wc -l)" -eq 4 ] || {
  printf '%s\n' "release must contain exactly the target and three default service units" >&2
  exit 1
}

CARILLON_CORE_BINARY="$RELEASE/bin/carillon" CARILLON_RUNTIME_BINARY="$RELEASE/bin/carillon-runtime-pi" \
  CARILLON_COMMANDS_MODULE="$RELEASE/lib/carillon_discord_commands.wasm" \
  node "$ROOT/scripts/test-integration.mjs"

TEST_ROOT=$(mktemp -d)
OWNER_HOME="$TEST_ROOT/owner home"
WORKSPACE=$OWNER_HOME/workspace
CARILLON_HOME="$TEST_ROOT/custom installation"
XDG_CONFIG_HOME=$OWNER_HOME/.config
GLOBAL_PI=$OWNER_HOME/.pi
mkdir -p "$WORKSPACE" "$GLOBAL_PI"
printf '%s\n' owner-global-pi-sentinel >"$GLOBAL_PI/sentinel"
printf '%s\n' unrelated-owner-file >"$OWNER_HOME/unrelated"
GLOBAL_PI_HASH=$(sha256sum "$GLOBAL_PI/sentinel" | cut -d' ' -f1)
OWNER=$(id -un)
OWNER_SHELL=$(getent passwd "$OWNER" | cut -d: -f7)
RUNTIME_PID=
CORE_PID=
DISCORD_PID=

stop_services() {
  for pid in "$DISCORD_PID" "$CORE_PID" "$RUNTIME_PID"; do
    [ -z "$pid" ] || kill "$pid" 2>/dev/null || true
  done
  for pid in "$DISCORD_PID" "$CORE_PID" "$RUNTIME_PID"; do
    [ -z "$pid" ] || wait "$pid" 2>/dev/null || true
  done
  RUNTIME_PID=
  CORE_PID=
  DISCORD_PID=
}
cleanup() {
  stop_services
  rm -rf "$TEST_ROOT"
}
trap cleanup EXIT INT TERM

# Package names cannot override the identity embedded by Cargo. Refuse before
# creating staging/output, including for a non-default manifest version.
sh "$ROOT/scripts/test-package.sh"
VERSION_FIXTURE=$TEST_ROOT/version-fixture
mkdir -p "$VERSION_FIXTURE/scripts"
cp "$ROOT/scripts/package-release.sh" "$VERSION_FIXTURE/scripts/"
printf '[workspace.package]\nversion = "1.2.3"\n' >"$VERSION_FIXTURE/Cargo.toml"
if "$VERSION_FIXTURE/scripts/package-release.sh" 9.8.7 "$VERSION_FIXTURE/release.tar.gz" \
  >"$TEST_ROOT/version.log" 2>&1; then
  echo 'packager accepted a version different from the embedded identity' >&2
  exit 1
fi
grep -F 'requested release version 9.8.7 differs from the Carillon crate version 1.2.3' "$TEST_ROOT/version.log" >/dev/null
[ ! -e "$VERSION_FIXTURE/target" ] && [ ! -e "$VERSION_FIXTURE/release.tar.gz" ]
[ "$("$RELEASE/bin/carillon" --version)" = "carillon $VERSION" ]

install_release() {
  install_home=$1
  install_workspace=$2
  install_root=$3
  install_config_home=$4
  install_port=$5
  env HOME="$install_home" XDG_CONFIG_HOME="$install_config_home" \
    CARILLON_HOME="$install_root" CARILLON_SKIP_SYSTEMD_RELOAD=1 \
    "$RELEASE/install.sh" \
    --guild-id 100 --bot-user-id 201 --user-id 200 --route 300:always:channel \
    --workspace "$install_workspace" --base-port "$install_port"
}

verify_units() {
  verify_dir=$1
  [ "$(find "$verify_dir" -maxdepth 1 -type f -name 'carillon*' | wc -l)" -eq 4 ] || {
    printf '%s\n' "installation must contain exactly the target and three default service units" >&2
    exit 1
  }
  systemd-analyze verify "$verify_dir"/carillon.target "$verify_dir"/carillon-*.service
}

assert_service_identity() {
  identity_dir=$1
  for service in "$identity_dir"/carillon-*.service; do
    grep -Fqx "Environment=\"USER=$OWNER\"" "$service"
    grep -Fqx "Environment=\"LOGNAME=$OWNER\"" "$service"
  done
}

# Compare resolved locations before creating any managed state, including when
# the owner's global Pi directory itself points elsewhere.
mkdir "$OWNER_HOME/other"
ln -s "$GLOBAL_PI" "$OWNER_HOME/pi-alias"
for global_location in directory symlink; do
  if [ "$global_location" = symlink ]; then
    mv "$GLOBAL_PI" "$OWNER_HOME/pi-target"
    ln -s "$OWNER_HOME/pi-target" "$GLOBAL_PI"
  fi
  for overlap in "$GLOBAL_PI" "$GLOBAL_PI/carillon" \
    "$OWNER_HOME/other/../.pi/carillon" "$OWNER_HOME/pi-alias/carillon" "$OWNER_HOME"; do
    if install_release "$OWNER_HOME" "$WORKSPACE" "$overlap" "$XDG_CONFIG_HOME" \
      "$BASE_PORT" >"$TEST_ROOT/overlap.log" 2>&1; then
      echo "installer accepted Pi overlap: $overlap" >&2
      exit 1
    fi
    grep -F 'must not overlap' "$TEST_ROOT/overlap.log" >/dev/null
    [ "$(sha256sum "$GLOBAL_PI/sentinel" | cut -d' ' -f1)" = "$GLOBAL_PI_HASH" ]
    [ "$(find -L "$GLOBAL_PI" -mindepth 1 | wc -l)" -eq 1 ]
    [ ! -e "$XDG_CONFIG_HOME" ]
  done
done

sh "$ROOT/scripts/test-maintenance.sh" "$RELEASE"
sh "$ROOT/scripts/test-operations.sh" "$RELEASE"

UNRELATED_HOME=$TEST_ROOT/unrelated-home
UNRELATED_WORKSPACE=$UNRELATED_HOME/workspace
UNRELATED_ROOT=$UNRELATED_HOME/.carillon
UNRELATED_CONFIG_HOME=$UNRELATED_HOME/.config
mkdir -p "$UNRELATED_WORKSPACE" "$UNRELATED_ROOT/app"
printf '%s\n' unrelated-application >"$UNRELATED_ROOT/app/sentinel"
if install_release "$UNRELATED_HOME" "$UNRELATED_WORKSPACE" "$UNRELATED_ROOT" \
  "$UNRELATED_CONFIG_HOME" "$((BASE_PORT + 20))" >/dev/null 2>&1
then
  echo "installer replaced an unowned application path" >&2
  exit 1
fi
[ "$(cat "$UNRELATED_ROOT/app/sentinel")" = unrelated-application ]
[ ! -e "$UNRELATED_ROOT/carillon.toml" ]
[ ! -e "$UNRELATED_CONFIG_HOME" ]

REGISTER_COMMANDS=$TEST_ROOT/register-commands
mkdir "$REGISTER_COMMANDS"
cat >"$REGISTER_COMMANDS/systemctl" <<'SYSTEMCTL'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$REGISTER_TRACE"
SYSTEMCTL
chmod 0755 "$REGISTER_COMMANDS/systemctl"
ORDINARY_HOME=$TEST_ROOT/ordinary-home
ORDINARY_WORKSPACE=$ORDINARY_HOME/workspace
ORDINARY_ROOT=$ORDINARY_HOME/.carillon
ORDINARY_CONFIG_HOME=$ORDINARY_HOME/.config
ORDINARY_UNIT_DIR=$ORDINARY_CONFIG_HOME/systemd/user
mkdir -p "$ORDINARY_WORKSPACE"
install_release "$ORDINARY_HOME" "$ORDINARY_WORKSPACE" "$ORDINARY_ROOT" \
  "$ORDINARY_CONFIG_HOME" "$((BASE_PORT + 10))" >/dev/null
verify_units "$ORDINARY_UNIT_DIR"
assert_service_identity "$ORDINARY_UNIT_DIR"
[ "$(cat "$ORDINARY_ROOT/app/.carillon-managed")" = carillon-managed-application-v1 ]
ORDINARY_TOKEN_HASH=$(sha256sum "$ORDINARY_ROOT/credentials/runtime-pi.token" | cut -d' ' -f1)
printf '%s\n' replaced-by-upgrade >"$ORDINARY_ROOT/app/upgrade-sentinel"
# The bundle owns its application tree. A provider installed independently
# goes on PATH instead, where replacing the bundle cannot remove it, and the
# single CLI forwards to it there.
PROVIDER_BIN=$TEST_ROOT/provider-bin
mkdir "$PROVIDER_BIN"
cat >"$PROVIDER_BIN/carillon-review" <<'PROVIDER'
#!/bin/sh
printf 'review-provider %s\n' "$*"
PROVIDER
chmod 0755 "$PROVIDER_BIN/carillon-review"
forward_review() {
  env PATH="$PROVIDER_BIN:$PATH" "$ORDINARY_ROOT/app/bin/carillon" review serve --flag
}
[ "$(forward_review)" = "review-provider serve --flag" ]
if "$ORDINARY_ROOT/app/bin/carillon" review serve >/dev/null 2>&1; then
  echo "forwarded to a provider that is not installed" >&2
  exit 1
fi
install_release "$ORDINARY_HOME" "$ORDINARY_WORKSPACE" "$ORDINARY_ROOT" \
  "$ORDINARY_CONFIG_HOME" "$((BASE_PORT + 10))" >/dev/null
[ ! -e "$ORDINARY_ROOT/app/upgrade-sentinel" ]
[ "$(forward_review)" = "review-provider serve --flag" ]
[ "$(sha256sum "$ORDINARY_ROOT/credentials/runtime-pi.token" | cut -d' ' -f1)" = "$ORDINARY_TOKEN_HASH" ]
ORDINARY_APP_HASH=$(sha256sum "$ORDINARY_ROOT/app/bin/carillon" | cut -d' ' -f1)
ORDINARY_CONFIG_HASH=$(sha256sum "$ORDINARY_ROOT/carillon.toml" | cut -d' ' -f1)
mv "$ORDINARY_ROOT/app/.carillon-managed" "$ORDINARY_ROOT/app/.carillon-managed.saved"
if install_release "$ORDINARY_HOME" "$ORDINARY_WORKSPACE" "$ORDINARY_ROOT" \
  "$ORDINARY_CONFIG_HOME" "$((BASE_PORT + 10))" >/dev/null 2>&1
then
  echo "upgrade replaced an application without its ownership marker" >&2
  exit 1
fi
[ "$(sha256sum "$ORDINARY_ROOT/app/bin/carillon" | cut -d' ' -f1)" = "$ORDINARY_APP_HASH" ]
[ "$(sha256sum "$ORDINARY_ROOT/carillon.toml" | cut -d' ' -f1)" = "$ORDINARY_CONFIG_HASH" ]
mv "$ORDINARY_ROOT/app/.carillon-managed.saved" "$ORDINARY_ROOT/app/.carillon-managed"
mv "$ORDINARY_ROOT/app/.carillon-managed" "$ORDINARY_ROOT/app/.carillon-managed.saved"
if env HOME="$ORDINARY_HOME" XDG_CONFIG_HOME="$ORDINARY_CONFIG_HOME" \
  CARILLON_HOME="$ORDINARY_ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=1 \
  PATH="$REGISTER_COMMANDS:$PATH" REGISTER_TRACE="$TEST_ROOT/ordinary-uninstall.trace" \
  "$ORDINARY_ROOT/app/bin/carillon-uninstall" >/dev/null 2>&1
then
  echo "uninstaller removed an application without its ownership marker" >&2
  exit 1
fi
[ -e "$ORDINARY_ROOT/app/bin/carillon" ]
[ -e "$ORDINARY_UNIT_DIR/carillon.target" ]
mv "$ORDINARY_ROOT/app/.carillon-managed.saved" "$ORDINARY_ROOT/app/.carillon-managed"
env HOME="$ORDINARY_HOME" XDG_CONFIG_HOME="$ORDINARY_CONFIG_HOME" \
  CARILLON_HOME="$ORDINARY_ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=1 \
  PATH="$REGISTER_COMMANDS:$PATH" REGISTER_TRACE="$TEST_ROOT/ordinary-uninstall.trace" \
  "$ORDINARY_ROOT/app/bin/carillon-uninstall" >/dev/null
[ ! -e "$ORDINARY_ROOT/app" ]
[ ! -e "$ORDINARY_UNIT_DIR/carillon.target" ]
[ -e "$ORDINARY_ROOT/carillon.toml" ]
[ "$(sha256sum "$ORDINARY_ROOT/credentials/runtime-pi.token" | cut -d' ' -f1)" = "$ORDINARY_TOKEN_HASH" ]

install_release "$OWNER_HOME" "$WORKSPACE" "$CARILLON_HOME" "$XDG_CONFIG_HOME" \
  "$BASE_PORT" >/dev/null
verify_units "$XDG_CONFIG_HOME/systemd/user"
assert_service_identity "$XDG_CONFIG_HOME/systemd/user"

[ "$(stat -c %u "$CARILLON_HOME")" -eq "$(id -u)" ]
[ "$(stat -c %a "$CARILLON_HOME")" = 700 ]
for unit in "$XDG_CONFIG_HOME/systemd/user"/carillon*; do
  [ "$(stat -c %u "$unit")" -eq "$(id -u)" ]
  [ "$(stat -c %a "$unit")" = 600 ]
done
[ "$(cat "$CARILLON_HOME/app/.carillon-managed")" = carillon-managed-application-v1 ]
[ "$(stat -c %a "$CARILLON_HOME/providers/discord-private.toml")" = 600 ]
grep -Fqx 'guild_id = "100"' "$CARILLON_HOME/providers/discord-private.toml"
[ "$(sha256sum "$GLOBAL_PI/sentinel" | cut -d' ' -f1)" = "$GLOBAL_PI_HASH" ]
[ ! -e "$GLOBAL_PI/auth.json" ]

# Exercise the packaged login entry without launching an interactive provider flow.
mv "$CARILLON_HOME/app/bin/pi" "$CARILLON_HOME/app/bin/pi.real"
cat >"$CARILLON_HOME/app/bin/pi" <<'PROBE'
#!/bin/sh
printf '%s\n%s\n' "$PI_CODING_AGENT_DIR" "$HOME" >"$CARILLON_HOME/pi/login-probe"
PROBE
chmod 0755 "$CARILLON_HOME/app/bin/pi"
ln -s "$CARILLON_HOME/app/bin/carillon-pi" "$TEST_ROOT/pi-login"
for launcher in "$CARILLON_HOME/app/bin/carillon-pi" "$TEST_ROOT/pi-login"; do
  for location in unset conflicting; do
    rm -f "$CARILLON_HOME/pi/login-probe"
    if [ "$location" = unset ]; then
      env -u CARILLON_HOME HOME="$OWNER_HOME" "$launcher"
    else
      env HOME="$OWNER_HOME" CARILLON_HOME="$TEST_ROOT/wrong-root" "$launcher"
    fi
    [ "$(sed -n '1p' "$CARILLON_HOME/pi/login-probe")" = "$CARILLON_HOME/pi" ]
    [ "$(sed -n '2p' "$CARILLON_HOME/pi/login-probe")" = "$OWNER_HOME" ]
    [ ! -e "$TEST_ROOT/wrong-root" ]
    [ ! -e "$OWNER_HOME/.carillon" ]
  done
done
mv "$CARILLON_HOME/app/bin/pi.real" "$CARILLON_HOME/app/bin/pi"

APP=$CARILLON_HOME/app/bin
CONFIG=$CARILLON_HOME/carillon.toml

start_runtime() {
  env HOME="$OWNER_HOME" USER="$OWNER" LOGNAME="$OWNER" SHELL="$OWNER_SHELL" PATH="$PATH" \
    CARILLON_HOME="$CARILLON_HOME" PI_CODING_AGENT_DIR="$CARILLON_HOME/pi" \
    "$APP/carillon-runtime-pi" --config "$CONFIG" --provider runtime-pi \
      --token-file "$CARILLON_HOME/credentials/runtime-pi.token" \
      --state-dir "$CARILLON_HOME/pi" --deterministic \
      >>"$CARILLON_HOME/logs/runtime-pi.log" 2>&1 &
  RUNTIME_PID=$!
}
start_runtime
env HOME="$OWNER_HOME" USER="$OWNER" LOGNAME="$OWNER" SHELL="$OWNER_SHELL" PATH="$PATH" \
  CARILLON_HOME="$CARILLON_HOME" PI_CODING_AGENT_DIR="$CARILLON_HOME/pi" \
  "$APP/carillon" core --config "$CONFIG" >>"$CARILLON_HOME/logs/core.log" 2>&1 &
CORE_PID=$!
env HOME="$OWNER_HOME" USER="$OWNER" LOGNAME="$OWNER" SHELL="$OWNER_SHELL" PATH="$PATH" \
  CARILLON_HOME="$CARILLON_HOME" PI_CODING_AGENT_DIR="$CARILLON_HOME/pi" \
  "$APP/carillon" discord serve --config "$CONFIG" --provider discord-private \
    --token-file "$CARILLON_HOME/credentials/discord-private.token" \
    --commands "$CARILLON_HOME/app/lib/carillon_discord_commands.wasm" --deterministic \
    >>"$CARILLON_HOME/logs/discord.log" 2>&1 &
DISCORD_PID=$!

for offset in 0 1 2; do
  port=$((BASE_PORT + offset))
  attempt=0
  until curl --fail --silent "http://127.0.0.1:$port/health" >/dev/null; do
    attempt=$((attempt + 1))
    if [ "$attempt" -ge 100 ]; then
      cat "$CARILLON_HOME/logs"/*.log >&2
      exit 1
    fi
    sleep 0.05
  done
done

grep -F -- '--provider runtime-pi' "$XDG_CONFIG_HOME/systemd/user/carillon-runtime-pi.service" >/dev/null
grep -F -- "--commands \"$CARILLON_HOME/app/lib/carillon_discord_commands.wasm\"" \
  "$XDG_CONFIG_HOME/systemd/user/carillon-discord.service" >/dev/null
RUNTIME_TOKEN=$(cat "$CARILLON_HOME/credentials/runtime-pi.token")
ENVIRONMENT=$(curl --fail --silent -H "authorization: Bearer $RUNTIME_TOKEN" \
  "http://127.0.0.1:$((BASE_PORT + 2))/v1/environment")
# The JavaScript reads these values from process.env; shell expansion would corrupt its template literals.
# shellcheck disable=SC2016
env EXPECTED_UID="$(id -u)" EXPECTED_USER="$OWNER" EXPECTED_HOME="$OWNER_HOME" \
  EXPECTED_SHELL="$OWNER_SHELL" EXPECTED_PATH="$PATH" EXPECTED_WORKSPACE="$WORKSPACE" \
  ENVIRONMENT="$ENVIRONMENT" node -e '
const value = JSON.parse(process.env.ENVIRONMENT);
for (const key of ["uid", "user", "home", "shell", "path", "workspace"]) {
  const expected = process.env[`EXPECTED_${key.toUpperCase()}`];
  if (String(value[key]) !== expected) throw new Error(`${key}: ${String(value[key])} != ${expected}`);
}
const expectedGroups = process.getgroups().slice().sort((a, b) => a - b);
const actualGroups = value.groups.slice().sort((a, b) => a - b);
if (JSON.stringify(actualGroups) !== JSON.stringify(expectedGroups)) throw new Error("supplementary groups differ");
'

DISCORD_PORT=$((BASE_PORT + 1))
# Keep the private credential out of curl's process arguments.
SIMULATION_HEADERS=$OWNER_HOME/simulation-headers
printf 'Authorization: Bearer %s\n' "$(cat "$CARILLON_HOME/credentials/discord-private.token")" > "$SIMULATION_HEADERS"
chmod 0600 "$SIMULATION_HEADERS"
DENIED=$(curl --silent --output /dev/null --write-out '%{http_code}' -X POST \
  -H 'content-type: application/json' -H "@$SIMULATION_HEADERS" \
  --data '{"event_id":"denied","guild_id":"100","user_id":"999","channel_id":"300","content":"must not run"}' \
  "http://127.0.0.1:$DISCORD_PORT/v1/simulate")
[ "$DENIED" = 403 ]

FIRST=$(curl --fail --silent -X POST -H 'content-type: application/json' \
  -H "@$SIMULATION_HEADERS" \
  --data '{"event_id":"installed-1","guild_id":"100","user_id":"200","channel_id":"300","content":"installed identity turn"}' \
  "http://127.0.0.1:$DISCORD_PORT/v1/simulate")
case "$FIRST" in *"Local provider response 1"*) ;; *) printf '%s\n' "$FIRST" >&2; exit 1 ;; esac
# The packaged command module answers slash commands through the packaged provider.
PANEL=$(curl --fail --silent -X POST -H 'content-type: application/json' \
  -H "@$SIMULATION_HEADERS" \
  --data '{"guild_id":"100","user_id":"200","channel_id":"300","kind":"command","name":"model"}' \
  "http://127.0.0.1:$DISCORD_PORT/v1/interactions")
case "$PANEL" in *"model:model:conversation"*) ;; *) printf '%s\n' "$PANEL" >&2; exit 1 ;; esac
kill "$RUNTIME_PID"
wait "$RUNTIME_PID" 2>/dev/null || true
RUNTIME_PID=
start_runtime
attempt=0
until curl --fail --silent "http://127.0.0.1:$((BASE_PORT + 2))/health" >/dev/null; do
  attempt=$((attempt + 1)); [ "$attempt" -lt 100 ] || exit 1; sleep 0.05
done
SECOND=$(curl --fail --silent -X POST -H 'content-type: application/json' \
  -H "@$SIMULATION_HEADERS" \
  --data '{"event_id":"installed-2","guild_id":"100","user_id":"200","channel_id":"300","content":"continue after restart"}' \
  "http://127.0.0.1:$DISCORD_PORT/v1/simulate")
case "$SECOND" in *"Local provider response 2"*) ;; *) printf '%s\n' "$SECOND" >&2; exit 1 ;; esac

stop_services
# Capture a real packaged installation before uninstall removes its application.
"$APP/carillon" --home "$CARILLON_HOME" backup --output "$TEST_ROOT/registration-backup.tar.gz" >/dev/null
cp -a "$XDG_CONFIG_HOME/systemd/user" "$TEST_ROOT/source-units"
env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" CARILLON_HOME="$CARILLON_HOME" \
  PATH="$REGISTER_COMMANDS:$PATH" REGISTER_TRACE="$TEST_ROOT/uninstall.trace" \
  CARILLON_SKIP_SYSTEMD_RELOAD=1 "$CARILLON_HOME/app/bin/carillon-uninstall" >/dev/null
printf '%s\n' '--user disable --now carillon.target' '--user daemon-reload' >"$TEST_ROOT/expected-uninstall"
cmp "$TEST_ROOT/expected-uninstall" "$TEST_ROOT/uninstall.trace"
[ ! -e "$CARILLON_HOME/app" ]
[ ! -e "$XDG_CONFIG_HOME/systemd/user/carillon.target" ]
[ "$(cat "$OWNER_HOME/unrelated")" = unrelated-owner-file ]
[ "$(sha256sum "$GLOBAL_PI/sentinel" | cut -d' ' -f1)" = "$GLOBAL_PI_HASH" ]
[ -e "$CARILLON_HOME/carillon.toml" ]

# Remove access through the original installation path. Restored registration
# uses only archived packaged content, never the release installer or checkout.
mv "$CARILLON_HOME" "$OWNER_HOME/uninstalled-source"
for previous in fresh existing symlink; do
  if [ "$previous" != existing ]; then
    RESTORE_HOME=$TEST_ROOT/restored-$previous-owner
    RESTORE_CONFIG_HOME=$RESTORE_HOME/.config
  else
    RESTORE_HOME=$OWNER_HOME
    RESTORE_CONFIG_HOME=$XDG_CONFIG_HOME
    cp -a "$TEST_ROOT/source-units/." "$RESTORE_CONFIG_HOME/systemd/user/"
  fi
  mkdir -p "$RESTORE_HOME"
  RESTORE_ROOT=$RESTORE_HOME/relocated-carillon
  RESTORE_UNITS=$RESTORE_CONFIG_HOME/systemd/user
  "$RELEASE/bin/carillon" restore --input "$TEST_ROOT/registration-backup.tar.gz" \
    --target "$RESTORE_ROOT" >/dev/null
  mkdir "$RESTORE_ROOT/private-before"
  cp -a "$RESTORE_ROOT/carillon.toml" "$RESTORE_ROOT/credentials" "$RESTORE_ROOT/providers" \
    "$RESTORE_ROOT/pi" "$RESTORE_ROOT/private-before/"
  REGISTER_TRACE=$TEST_ROOT/register-$previous.trace
  REGISTER_HELPER=$RESTORE_ROOT/app/bin/carillon-register-services
  if [ "$previous" = symlink ]; then
    # The link lives inside another valid installation: deriving its directory
    # would silently publish units for that decoy, not merely fail to find config.
    REGISTER_HELPER=$TEST_ROOT/restored-fresh-owner/relocated-carillon/app/bin/register-link
    ln -s "$RESTORE_ROOT/app/bin/carillon-register-services" "$REGISTER_HELPER"
  fi
  # A stale inherited CARILLON_HOME must not redirect a relocated helper.
  env HOME="$RESTORE_HOME" XDG_CONFIG_HOME="$RESTORE_CONFIG_HOME" \
    CARILLON_HOME="$CARILLON_HOME" CARILLON_SKIP_SYSTEMD_RELOAD=0 \
    PATH="$REGISTER_COMMANDS:/usr/bin:/bin" REGISTER_TRACE="$REGISTER_TRACE" \
    "$REGISTER_HELPER" >/dev/null
  printf '%s\n' '--user daemon-reload' '--user enable carillon.target' >"$TEST_ROOT/expected-registration"
  cmp "$TEST_ROOT/expected-registration" "$REGISTER_TRACE"
  verify_units "$RESTORE_UNITS"
  assert_service_identity "$RESTORE_UNITS"
  for service in "$RESTORE_UNITS"/carillon-*.service; do
    grep -Fqx "Environment=\"HOME=$RESTORE_HOME\"" "$service"
    grep -Fqx "Environment=\"CARILLON_HOME=$RESTORE_ROOT\"" "$service"
    grep -Fqx "Environment=\"SHELL=$OWNER_SHELL\"" "$service"
    grep -Fqx "Environment=\"PATH=$REGISTER_COMMANDS:/usr/bin:/bin\"" "$service"
    grep -F -- "\"$RESTORE_ROOT/app/bin/" "$service" >/dev/null
    if grep -F "$CARILLON_HOME" "$service"; then
      echo 'restored service still refers to the source installation' >&2
      exit 1
    fi
    [ "$(stat -c %u "$service")" -eq "$(id -u)" ]
    [ "$(stat -c %a "$service")" = 600 ]
  done
  cmp "$RESTORE_ROOT/private-before/carillon.toml" "$RESTORE_ROOT/carillon.toml"
  diff -r "$RESTORE_ROOT/private-before/credentials" "$RESTORE_ROOT/credentials"
  diff -r "$RESTORE_ROOT/private-before/providers" "$RESTORE_ROOT/providers"
  diff -r "$RESTORE_ROOT/private-before/pi" "$RESTORE_ROOT/pi"
  [ ! -e "$CARILLON_HOME" ]
  printf 'restored service registration passed: %s units, relocated root and owner environment\n' "$previous"
done

sh "$ROOT/scripts/test-uninstall.sh" "$RELEASE"

printf '%s\n' "install tests passed: owner authority, admission, continuation, private Pi state, and unrelated files"
