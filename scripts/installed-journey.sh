#!/bin/sh
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
RELEASE=${1:-"$ROOT/target/package/carillon-0.0.0"}
BASE_PORT=${CARILLON_TEST_BASE_PORT:-44800}

for artifact in \
  bin/carillon \
  bin/carillon-runtime-pi \
  bin/pi \
  bin/carillon-pi \
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
DENIED=$(curl --silent --output /dev/null --write-out '%{http_code}' -X POST \
  -H 'content-type: application/json' \
  --data '{"event_id":"denied","guild_id":"100","user_id":"999","channel_id":"300","content":"must not run"}' \
  "http://127.0.0.1:$DISCORD_PORT/v1/simulate")
[ "$DENIED" = 403 ]

FIRST=$(curl --fail --silent -X POST -H 'content-type: application/json' \
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
  --data '{"event_id":"installed-2","guild_id":"100","user_id":"200","channel_id":"300","content":"continue after restart"}' \
  "http://127.0.0.1:$DISCORD_PORT/v1/simulate")
case "$SECOND" in *"Local provider response 2"*) ;; *) printf '%s\n' "$SECOND" >&2; exit 1 ;; esac

stop_services
env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" CARILLON_HOME="$CARILLON_HOME" \
  CARILLON_SKIP_SYSTEMD_RELOAD=1 "$CARILLON_HOME/app/bin/carillon-uninstall" >/dev/null
[ ! -e "$CARILLON_HOME/app" ]
[ ! -e "$XDG_CONFIG_HOME/systemd/user/carillon.target" ]
[ "$(cat "$OWNER_HOME/unrelated")" = unrelated-owner-file ]
[ "$(sha256sum "$GLOBAL_PI/sentinel" | cut -d' ' -f1)" = "$GLOBAL_PI_HASH" ]
[ -e "$CARILLON_HOME/carillon.toml" ]

printf '%s\n' "installed per-user journey preserved owner authority, admission, continuation, private Pi state, and unrelated files"
