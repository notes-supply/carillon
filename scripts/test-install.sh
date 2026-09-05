#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
RELEASE=${1:-"$ROOT/target/package/carillon-0.0.0"}
BASE_PORT=${CARILLON_TEST_BASE_PORT:-44800}

# This tests the bundled Discord/Pi release, not a limit on provider extensibility.
# Reinstallation below replaces binaries with the same schema; it is not a
# database/configuration migration or a promise to support old beta formats.

for artifact in \
  bin/carillon \
  bin/carillon-runtime-pi \
  bin/pi \
  bin/carillon-pi \
  bin/carillon-register-services \
  install.sh \
  uninstall.sh \
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

node "$ROOT/scripts/test-filesystem.mjs" init "$RELEASE"
CARILLON_CORE_BINARY="$RELEASE/bin/carillon" CARILLON_RUNTIME_BINARY="$RELEASE/bin/carillon-runtime-pi" \
  node "$ROOT/scripts/test-integration.mjs"

TEST_ROOT=$(mktemp -d)
OWNER_HOME="$TEST_ROOT/owner home"
WORKSPACE=$OWNER_HOME/workspace
CARILLON_HOME=$OWNER_HOME/.carillon
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

# Fail only unit operations, after earlier templates have been processed. Use
# real package binaries and filesystem writes; never contact the service manager.
FAIL_COMMANDS=$TEST_ROOT/fail-commands
mkdir "$FAIL_COMMANDS"
REAL_SED=$(command -v sed)
REAL_CHMOD=$(command -v chmod)
REAL_MV=$(command -v mv)
cat >"$FAIL_COMMANDS/fail-unit-command" <<'FAIL_COMMAND'
#!/bin/sh
set -eu
command=${0##*/}
last=
for argument do last=$argument; done
case "$command:$FAIL_MODE:$last" in
  mv:app-*:"${FAIL_APP:-}")
    printf '%s\n' "$FAIL_MODE" >"$FAIL_MARKER"
    # Even immediately before the rename the previous tree is still selected.
    [ -f "$FAIL_APP/upgrade-sentinel" ]
    case "$FAIL_MODE" in
      app-before) exit 1 ;;
      app-term-before) kill -TERM "$PPID"; exit 1 ;;
      app-kill-before) kill -KILL "$PPID"; exit 1 ;;
    esac
    "$REAL_MV" "$@"
    [ -x "$FAIL_APP/bin/carillon" ]
    [ ! -e "$FAIL_APP/upgrade-sentinel" ]
    case "$FAIL_MODE" in
      app-after) exit 1 ;;
      app-term-after) kill -TERM "$PPID"; exit 1 ;;
      app-kill-after) kill -KILL "$PPID"; exit 1 ;;
    esac
    exit 2
    ;;
  sed:render:"$FAIL_TEMPLATE")
    printf '%s\n' render >"$FAIL_MARKER"
    printf '%s\n' 'incomplete unit definition'
    exit 1
    ;;
  sed:write:"$FAIL_TEMPLATE")
    printf '%s\n' write >"$FAIL_MARKER"
    # Force the real renderer's output write to fail, not an unrelated command.
    ulimit -c 0
    ulimit -f 0
    exec "$REAL_SED" "$@"
    ;;
  chmod:chmod:"$FAIL_UNIT_DIR"/*/new/carillon.target | chmod:chmod:"$FAIL_UNIT_DIR/carillon.target")
    printf '%s\n' chmod >"$FAIL_MARKER"
    exit 1
    ;;
  mv:publish:"$FAIL_UNIT_DIR/carillon.target")
    # Publication really is partial: earlier new units exist, and the previous
    # runtime symlink (when present) has already been replaced by a regular file.
    [ -f "$FAIL_UNIT_DIR/carillon-discord.service" ]
    [ -f "$FAIL_UNIT_DIR/carillon-core.service" ]
    [ ! -L "$FAIL_UNIT_DIR/carillon-runtime-pi.service" ]
    printf '%s\n' publish >"$FAIL_MARKER"
    exit 1
    ;;
  mv:publish-after:"$FAIL_UNIT_DIR/carillon.target")
    if [ ! -e "$FAIL_MARKER" ]; then
      "$REAL_MV" "$@"
      printf '%s\n' publish-after >"$FAIL_MARKER"
      exit 1
    fi
    ;;
  systemctl:reload:daemon-reload | systemctl:enable:carillon.target)
    printf '%s\n' "$FAIL_MODE" >"$FAIL_MARKER"
    exit 1
    ;;
esac
case "$command" in
  sed) exec "$REAL_SED" "$@" ;;
  chmod) exec "$REAL_CHMOD" "$@" ;;
  mv) exec "$REAL_MV" "$@" ;;
  systemctl) exit 0 ;;
esac
exit 2
FAIL_COMMAND
chmod 0755 "$FAIL_COMMANDS/fail-unit-command"
for command in sed chmod mv systemctl; do
  ln -s fail-unit-command "$FAIL_COMMANDS/$command"
done

for previous in fresh existing; do
  FAILURE_HOME=$TEST_ROOT/unit-failure-$previous
  FAILURE_WORKSPACE=$FAILURE_HOME/workspace
  FAILURE_ROOT=$FAILURE_HOME/.carillon
  FAILURE_CONFIG_HOME=$FAILURE_HOME/.config
  FAILURE_UNIT_DIR=$FAILURE_CONFIG_HOME/systemd/user
  mkdir -p "$FAILURE_WORKSPACE" "$FAILURE_UNIT_DIR/unrelated.service.d"
  # Make preservation observable even when the test runner's umask is 077.
  chmod 0755 "$FAILURE_UNIT_DIR"
  printf '%s\n' unrelated-unit >"$FAILURE_UNIT_DIR/unrelated.service"
  printf '%s\n' unrelated-drop-in >"$FAILURE_UNIT_DIR/unrelated.service.d/override.conf"
  if [ "$previous" = existing ]; then
    install_release "$FAILURE_HOME" "$FAILURE_WORKSPACE" "$FAILURE_ROOT" \
      "$FAILURE_CONFIG_HOME" "$((BASE_PORT + 30))" >/dev/null
    printf '%s\n' previous-application >"$FAILURE_ROOT/app/upgrade-sentinel"
    old_inode=$(stat -c %i "$FAILURE_ROOT/app")
    old_token_hash=$(sha256sum "$FAILURE_ROOT/credentials/runtime-pi.token" | cut -d' ' -f1)
    printf '%s\n' previous-core >"$FAILURE_UNIT_DIR/carillon-core.service"
    printf '%s\n' previous-target >"$FAILURE_UNIT_DIR/carillon.target"
    chmod 0640 "$FAILURE_UNIT_DIR/carillon-core.service"
    chmod 0600 "$FAILURE_UNIT_DIR/carillon.target"
    rm "$FAILURE_UNIT_DIR/carillon-runtime-pi.service" "$FAILURE_UNIT_DIR/carillon-discord.service"
    ln -s unrelated.service "$FAILURE_UNIT_DIR/carillon-runtime-pi.service"
    # Leave Discord absent to exercise removal of a newly published unit.
  fi
  cp -a "$FAILURE_UNIT_DIR" "$FAILURE_HOME/units-before"
  find "$FAILURE_UNIT_DIR" -printf '%P %y %m %l\n' | sort >"$FAILURE_HOME/modes-before"
  for failure in render write chmod publish publish-after; do
    failure_marker=$FAILURE_HOME/$failure.triggered
    if env HOME="$FAILURE_HOME" XDG_CONFIG_HOME="$FAILURE_CONFIG_HOME" \
      CARILLON_HOME="$FAILURE_ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=1 \
      PATH="$FAIL_COMMANDS:$PATH" FAIL_MODE="$failure" FAIL_MARKER="$failure_marker" \
      FAIL_TEMPLATE="$FAILURE_ROOT/app/systemd/carillon.target" FAIL_UNIT_DIR="$FAILURE_UNIT_DIR" \
      REAL_SED="$REAL_SED" REAL_CHMOD="$REAL_CHMOD" REAL_MV="$REAL_MV" \
      "$RELEASE/install.sh" \
      --guild-id 100 --bot-user-id 201 --user-id 200 --route 300:always:channel \
      --workspace "$FAILURE_WORKSPACE" --base-port "$((BASE_PORT + 30))" \
      >"$FAILURE_HOME/$failure.log" 2>&1
    then
      echo "installer unexpectedly succeeded after $previous unit $failure failure" >&2
      exit 1
    fi
    [ -f "$failure_marker" ] || {
      cat "$FAILURE_HOME/$failure.log" >&2
      echo "unit $failure failure was not exercised" >&2
      exit 1
    }
    [ "$(cat "$failure_marker")" = "$failure" ]
    diff -r --no-dereference "$FAILURE_HOME/units-before" "$FAILURE_UNIT_DIR"
    find "$FAILURE_UNIT_DIR" -printf '%P %y %m %l\n' | sort >"$FAILURE_HOME/modes-after"
    cmp "$FAILURE_HOME/modes-before" "$FAILURE_HOME/modes-after"
    if [ "$previous" = existing ]; then
      [ "$(stat -c %i "$FAILURE_ROOT/app")" = "$old_inode" ] || {
        echo "previous application identity not restored after unit $failure failure" >&2
        exit 1
      }
      [ "$(cat "$FAILURE_ROOT/app/upgrade-sentinel")" = previous-application ]
      [ "$(sha256sum "$FAILURE_ROOT/credentials/runtime-pi.token" | cut -d' ' -f1)" = "$old_token_hash" ]
    fi
    printf 'install unit failure test passed: %s / %s\n' "$previous" "$failure"
  done
  # The same installation remains usable after each failed attempt.
  install_release "$FAILURE_HOME" "$FAILURE_WORKSPACE" "$FAILURE_ROOT" \
    "$FAILURE_CONFIG_HOME" "$((BASE_PORT + 30))" >/dev/null
  verify_units "$FAILURE_UNIT_DIR"
  assert_service_identity "$FAILURE_UNIT_DIR"
  cmp "$FAILURE_HOME/units-before/unrelated.service" "$FAILURE_UNIT_DIR/unrelated.service"
  diff -r "$FAILURE_HOME/units-before/unrelated.service.d" "$FAILURE_UNIT_DIR/unrelated.service.d"
done

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
# Fail or kill only this temporary installer on either side of publication.
# No rollback is needed for an atomic exchange: APP always selects a full tree.
for failure in app-before app-after app-term-before app-term-after app-kill-before app-kill-after; do
  printf '%s\n' replaced-by-upgrade >"$ORDINARY_ROOT/app/upgrade-sentinel"
  old_inode=$(stat -c %i "$ORDINARY_ROOT/app")
  failure_marker=$ORDINARY_HOME/$failure.triggered
  if env HOME="$ORDINARY_HOME" XDG_CONFIG_HOME="$ORDINARY_CONFIG_HOME" \
    CARILLON_HOME="$ORDINARY_ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=1 \
    PATH="$FAIL_COMMANDS:$PATH" FAIL_MODE="$failure" FAIL_MARKER="$failure_marker" \
    FAIL_APP="$ORDINARY_ROOT/app" FAIL_TEMPLATE="$RELEASE/systemd/carillon.target" \
    FAIL_UNIT_DIR="$ORDINARY_UNIT_DIR" REAL_MV="$REAL_MV" REAL_SED="$REAL_SED" REAL_CHMOD="$REAL_CHMOD" \
    "$RELEASE/install.sh" --guild-id 100 --bot-user-id 201 --user-id 200 \
    --route 300:always:channel --workspace "$ORDINARY_WORKSPACE" --base-port "$((BASE_PORT + 10))" \
    >"$ORDINARY_HOME/$failure.log" 2>&1; then
    echo "installer unexpectedly succeeded after $failure" >&2
    exit 1
  fi
  [ "$(cat "$failure_marker")" = "$failure" ]
  for original in "$RELEASE"/bin/*; do
    diff -r --no-dereference "$original" "$ORDINARY_ROOT/app/bin/$(basename "$original")"
  done
  [ "$(cat "$ORDINARY_ROOT/app/.carillon-managed")" = carillon-managed-application-v1 ]
  case "$failure" in
    *-before)
      [ "$(stat -c %i "$ORDINARY_ROOT/app")" = "$old_inode" ]
      [ -f "$ORDINARY_ROOT/app/upgrade-sentinel" ] ;;
    *-after)
      [ "$(stat -c %i "$ORDINARY_ROOT/app")" != "$old_inode" ]
      [ ! -e "$ORDINARY_ROOT/app/upgrade-sentinel" ] ;;
  esac
  [ "$(sha256sum "$ORDINARY_ROOT/credentials/runtime-pi.token" | cut -d' ' -f1)" = "$ORDINARY_TOKEN_HASH" ]
  install_release "$ORDINARY_HOME" "$ORDINARY_WORKSPACE" "$ORDINARY_ROOT" \
    "$ORDINARY_CONFIG_HOME" "$((BASE_PORT + 10))" >/dev/null
  [ ! -e "$ORDINARY_ROOT/app/upgrade-sentinel" ]
  printf 'install application failure test passed: %s\n' "$failure"
done
for failure in reload enable; do
  printf '%s\n' previous-application >"$ORDINARY_ROOT/app/upgrade-sentinel"
  printf '%s\n' '# previous-unit' >>"$ORDINARY_UNIT_DIR/carillon-core.service"
  old_inode=$(stat -c %i "$ORDINARY_ROOT/app")
  failure_marker=$ORDINARY_HOME/$failure.triggered
  if env HOME="$ORDINARY_HOME" XDG_CONFIG_HOME="$ORDINARY_CONFIG_HOME" \
    CARILLON_HOME="$ORDINARY_ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=0 \
    PATH="$FAIL_COMMANDS:$PATH" FAIL_MODE="$failure" FAIL_MARKER="$failure_marker" \
    FAIL_TEMPLATE="$RELEASE/systemd/carillon.target" FAIL_UNIT_DIR="$ORDINARY_UNIT_DIR" \
    REAL_MV="$REAL_MV" REAL_SED="$REAL_SED" REAL_CHMOD="$REAL_CHMOD" \
    "$RELEASE/install.sh" --guild-id 100 --bot-user-id 201 --user-id 200 \
    --route 300:always:channel --workspace "$ORDINARY_WORKSPACE" --base-port "$((BASE_PORT + 10))" \
    >"$ORDINARY_HOME/$failure.log" 2>&1; then
    echo "installer unexpectedly succeeded after systemctl $failure failure" >&2
    exit 1
  fi
  [ "$(cat "$failure_marker")" = "$failure" ]
  [ "$(stat -c %i "$ORDINARY_ROOT/app")" != "$old_inode" ] || {
    echo "committed application incorrectly rolled back after systemctl $failure failure" >&2
    exit 1
  }
  [ ! -e "$ORDINARY_ROOT/app/upgrade-sentinel" ]
  if grep -F '# previous-unit' "$ORDINARY_UNIT_DIR/carillon-core.service"; then
    echo "old units were selected after committed registration" >&2
    exit 1
  fi
  [ "$(sha256sum "$ORDINARY_ROOT/credentials/runtime-pi.token" | cut -d' ' -f1)" = "$ORDINARY_TOKEN_HASH" ]
  retained=0
  for candidate in "$ORDINARY_ROOT"/.app-new.*; do
    [ "$(stat -c %i "$candidate")" != "$old_inode" ] || retained=1
  done
  [ "$retained" -eq 1 ]
  verify_units "$ORDINARY_UNIT_DIR"
  install_release "$ORDINARY_HOME" "$ORDINARY_WORKSPACE" "$ORDINARY_ROOT" \
    "$ORDINARY_CONFIG_HOME" "$((BASE_PORT + 10))" >/dev/null
  printf 'install committed registration failure test passed: %s\n' "$failure"
done
printf '%s\n' replaced-by-upgrade >"$ORDINARY_ROOT/app/upgrade-sentinel"
install_release "$ORDINARY_HOME" "$ORDINARY_WORKSPACE" "$ORDINARY_ROOT" \
  "$ORDINARY_CONFIG_HOME" "$((BASE_PORT + 10))" >/dev/null
[ ! -e "$ORDINARY_ROOT/app/upgrade-sentinel" ]
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
[ "$(sha256sum "$GLOBAL_PI/sentinel" | cut -d' ' -f1)" = "$GLOBAL_PI_HASH" ]
[ ! -e "$GLOBAL_PI/auth.json" ]

# Exercise the packaged login entry without launching an interactive provider flow.
mv "$CARILLON_HOME/app/bin/pi" "$CARILLON_HOME/app/bin/pi.real"
cat >"$CARILLON_HOME/app/bin/pi" <<'PROBE'
#!/bin/sh
printf '%s\n%s\n' "$PI_CODING_AGENT_DIR" "$HOME" >"$CARILLON_HOME/pi/login-probe"
PROBE
chmod 0755 "$CARILLON_HOME/app/bin/pi"
env HOME="$OWNER_HOME" CARILLON_HOME="$CARILLON_HOME" "$CARILLON_HOME/app/bin/carillon-pi"
[ "$(sed -n '1p' "$CARILLON_HOME/pi/login-probe")" = "$CARILLON_HOME/pi" ]
[ "$(sed -n '2p' "$CARILLON_HOME/pi/login-probe")" = "$OWNER_HOME" ]
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
  "$APP/carillon" discord --config "$CONFIG" \
    --token-file "$CARILLON_HOME/credentials/discord-private.token" --deterministic \
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
"$APP/carillon" backup --config "$CONFIG" --output "$TEST_ROOT/registration-backup.tar.gz" >/dev/null
cp -a "$XDG_CONFIG_HOME/systemd/user" "$TEST_ROOT/source-units"
env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" CARILLON_HOME="$CARILLON_HOME" \
  CARILLON_SKIP_SYSTEMD_RELOAD=1 "$CARILLON_HOME/app/bin/carillon-uninstall" >/dev/null
[ ! -e "$CARILLON_HOME/app" ]
[ ! -e "$XDG_CONFIG_HOME/systemd/user/carillon.target" ]
[ "$(cat "$OWNER_HOME/unrelated")" = unrelated-owner-file ]
[ "$(sha256sum "$GLOBAL_PI/sentinel" | cut -d' ' -f1)" = "$GLOBAL_PI_HASH" ]
[ -e "$CARILLON_HOME/carillon.toml" ]

# Remove access through the original installation path. Restored registration
# uses only archived packaged content, never the release installer or checkout.
mv "$CARILLON_HOME" "$OWNER_HOME/uninstalled-source"
REGISTER_COMMANDS=$TEST_ROOT/register-commands
mkdir "$REGISTER_COMMANDS"
cat >"$REGISTER_COMMANDS/systemctl" <<'SYSTEMCTL'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$REGISTER_TRACE"
SYSTEMCTL
chmod 0755 "$REGISTER_COMMANDS/systemctl"
for previous in fresh existing; do
  if [ "$previous" = fresh ]; then
    RESTORE_HOME=$TEST_ROOT/restored-owner
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
  cp -a "$RESTORE_ROOT/carillon.toml" "$RESTORE_ROOT/credentials" "$RESTORE_ROOT/pi" \
    "$RESTORE_ROOT/private-before/"
  REGISTER_TRACE=$TEST_ROOT/register-$previous.trace
  # A stale inherited CARILLON_HOME must not redirect a relocated helper.
  env HOME="$RESTORE_HOME" XDG_CONFIG_HOME="$RESTORE_CONFIG_HOME" \
    CARILLON_HOME="$CARILLON_HOME" CARILLON_SKIP_SYSTEMD_RELOAD=0 \
    PATH="$REGISTER_COMMANDS:/usr/bin:/bin" REGISTER_TRACE="$REGISTER_TRACE" \
    "$RESTORE_ROOT/app/bin/carillon-register-services" >/dev/null
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
  diff -r "$RESTORE_ROOT/private-before/pi" "$RESTORE_ROOT/pi"
  [ ! -e "$CARILLON_HOME" ]
  printf 'restored service registration passed: %s units, relocated root and owner environment\n' "$previous"
done

printf '%s\n' "install tests passed: owner authority, admission, continuation, private Pi state, and unrelated files"
