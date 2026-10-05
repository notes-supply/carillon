#!/bin/sh
set -eu

# Everyday operation of installed releases through the owner's PATH entry point:
# installation resolution, service control, logs, provider commands, and
# configuration-preserving updates with their refusals and failures. The user
# service manager is substituted by one that runs the packaged services with
# deterministic Discord and Pi, so health checks exercise the real programs.
RELEASE=$(realpath -e -- "${1:?release directory required}")
VERSION=${RELEASE##*/carillon-}
BASE_PORT=$((${CARILLON_TEST_BASE_PORT:-44800} + 30))
ALT_PORT=$((BASE_PORT + 10))
TEST_ROOT=$(mktemp -d)
COMMANDS=$TEST_ROOT/commands
MANAGER=$TEST_ROOT/manager
TRACE=$TEST_ROOT/systemctl.trace
HOME=$TEST_ROOT/owner
XDG_CONFIG_HOME=$HOME/.config
PATH=$HOME/.local/bin:$COMMANDS:$PATH
INSTALLATION=$HOME/.carillon
ALTERNATE=$TEST_ROOT/alternate
ENTRY=$HOME/.local/bin/carillon
export HOME XDG_CONFIG_HOME PATH MANAGER TRACE
unset CARILLON_HOME CARILLON_MAINTENANCE_LOCK CARILLON_SKIP_SYSTEMD_RELOAD
mkdir -p "$HOME/workspace" "$COMMANDS" "$MANAGER"

cat >"$COMMANDS/systemctl" <<'SYSTEMCTL'
#!/bin/sh
set -eu
printf '%s\n' "$*" >>"$TRACE"
units=$XDG_CONFIG_HOME/systemd/user
running() { [ -s "$MANAGER/pids" ]; }
start() {
  [ "${FAKE_START:-ok}" != fail ] || exit 1
  root=$(sed -n 's/^Environment="CARILLON_HOME=\(.*\)"$/\1/p' "$units/carillon-core.service")
  bin=$root/app/bin
  config=$root/carillon.toml
  env CARILLON_HOME="$root" PI_CODING_AGENT_DIR="$root/pi" "$bin/carillon-runtime-pi" \
    --config "$config" --provider runtime-pi --token-file "$root/credentials/runtime-pi.token" \
    --state-dir "$root/pi" --deterministic >>"$root/logs/runtime-pi.log" 2>&1 &
  echo "$!" >>"$MANAGER/pids"
  env CARILLON_HOME="$root" "$bin/carillon" core --config "$config" >>"$root/logs/core.log" 2>&1 &
  echo "$!" >>"$MANAGER/pids"
  if [ "${FAKE_START:-ok}" != no-discord ]; then
    env CARILLON_HOME="$root" "$bin/carillon" discord serve --config "$config" \
      --provider discord-private --token-file "$root/credentials/discord-private.token" \
      --commands "$root/app/lib/carillon_discord_commands.wasm" --deterministic \
      >>"$root/logs/discord.log" 2>&1 &
    echo "$!" >>"$MANAGER/pids"
  fi
}
stop() {
  if [ "${FAKE_STOP:-ok}" = break-installer ]; then
    # Make the already checked installer unrunnable once services are down.
    root=$(sed -n 's/^Environment="CARILLON_HOME=\(.*\)"$/\1/p' "$units/carillon-core.service")
    chmod a-x "$root"/.update-*/carillon-*/install.sh
  fi
  running || return 0
  for pid in $(cat "$MANAGER/pids"); do kill "$pid" 2>/dev/null || true; done
  for pid in $(cat "$MANAGER/pids"); do
    while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
  done
  rm -f "$MANAGER/pids"
}
case "$*" in
  '--user show --property=ActiveState --value carillon.target')
    if running; then echo active; else echo inactive; fi ;;
  '--user start carillon.target') running || start ;;
  '--user stop carillon.target') stop ;;
  '--user restart carillon.target') stop; start ;;
  '--user daemon-reload') [ "${FAKE_RELOAD:-ok}" != fail ] || exit 1 ;;
  '--user disable --now carillon.target') stop ;;
  '--user enable carillon.target') ;;
  *) exit 2 ;;
esac
SYSTEMCTL
chmod 0755 "$COMMANDS/systemctl"

cleanup() {
  result=$?
  systemctl --user stop carillon.target >/dev/null 2>&1 || true
  if [ "$result" -ne 0 ]; then
    for log in "$TEST_ROOT"/*.log "$INSTALLATION"/logs/*.log; do
      [ ! -f "$log" ] || { printf '==> %s <==\n' "$log" >&2; tail -n 20 "$log" >&2; }
    done
  fi
  rm -rf -- "$TEST_ROOT"
  exit "$result"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM

fail() {
  printf '%s\n' "$*" >&2
  exit 1
}
# Run a command that must fail, keeping its output in the named log.
refuse() {
  log=$TEST_ROOT/$1.log
  shift
  if "$@" >"$log" 2>&1; then
    cat "$log" >&2
    fail "unexpectedly succeeded: $*"
  fi
}
running() { [ -s "$MANAGER/pids" ]; }
stopped() { ! running; }
# install_release PORT [VARIABLE=VALUE ...]
install_release() {
  port=$1
  shift
  env "$@" "$RELEASE/install.sh" --guild-id 100 --bot-user-id 201 --user-id 200 \
    --route 300:always:channel --workspace "$HOME/workspace" --base-port "$port"
}
simulate() {
  printf 'Authorization: Bearer %s\n' "$(cat "$INSTALLATION/credentials/discord-private.token")" \
    >"$TEST_ROOT/headers"
  curl --fail --silent -X POST -H 'content-type: application/json' -H "@$TEST_ROOT/headers" \
    --data "{\"event_id\":\"$1\",\"guild_id\":\"100\",\"user_id\":\"200\",\"channel_id\":\"300\",\"content\":\"$1\"}" \
    "http://127.0.0.1:$((BASE_PORT + 1))/v1/simulate"
}
release_archive() {
  name=$1
  directory=$TEST_ROOT/releases/$name
  mkdir -p "$directory"
  cp -a "$RELEASE" "$directory/carillon-$VERSION"
  printf '%s\n' "$name" >"$directory/carillon-$VERSION/lib/release-sentinel"
  printf '%s\n' 'r lib/release-sentinel' >>"$directory/carillon-$VERSION/MANIFEST"
  printf '%s\n' "$directory/carillon-$VERSION"
}
pack() {
  tar -C "$(dirname -- "$1")" -czf "$2" "$(basename -- "$1")"
}
snapshot() {
  rm -rf "$TEST_ROOT/before"
  mkdir "$TEST_ROOT/before"
  cp -a "$INSTALLATION/carillon.toml" "$INSTALLATION/providers" "$INSTALLATION/credentials" \
    "$INSTALLATION/pi" "$INSTALLATION/agents" "$TEST_ROOT/before/"
}
assert_preserved() {
  cmp "$TEST_ROOT/before/carillon.toml" "$INSTALLATION/carillon.toml"
  for tree in providers credentials pi; do diff -r "$TEST_ROOT/before/$tree" "$INSTALLATION/$tree"; done
  diff -r "$TEST_ROOT/before/agents/default/SYSTEM.md" "$INSTALLATION/agents/default/SYSTEM.md"
  [ -z "$(find "$INSTALLATION" -maxdepth 1 -name '.update-*')" ]
}
expect_trace() {
  printf '%s\n' "$@" >"$TEST_ROOT/expected.trace"
  cmp "$TEST_ROOT/expected.trace" "$TRACE" || { cat "$TRACE" >&2; fail 'unexpected service-manager calls'; }
  rm -f "$TRACE"
}

# First-time setup at the default location claims the owner's entry point.
install_release "$BASE_PORT" >"$TEST_ROOT/install.log"
[ "$(readlink "$ENTRY")" = "$INSTALLATION/app/bin/carillon" ]
[ "$(command -v carillon)" = "$ENTRY" ]
grep -Fx '3. Start deliberately with: carillon start' "$TEST_ROOT/install.log" >/dev/null
rm -f "$TRACE"

refuse stopped-status carillon status
grep -F 'carillon start' "$TEST_ROOT/stopped-status.log" >/dev/null
carillon start >"$TEST_ROOT/start.log"
grep -Fx '  discord-private: ready (0.0.0)' "$TEST_ROOT/start.log" >/dev/null
expect_trace '--user start carillon.target'
# Resolution never consults the current directory, even one that looks like an installation.
mkdir "$TEST_ROOT/decoy"
printf '%s\n' 'not a registry' >"$TEST_ROOT/decoy/carillon.toml"
(cd "$TEST_ROOT/decoy" && carillon status) | grep -F '"status": "ready"' >/dev/null
case "$(simulate installed-1)" in *'Local provider response 1'*) ;; *) fail 'first turn failed' ;; esac

# Provider commands receive the resolved installation and infer the only Discord instance.
MODULE=$INSTALLATION/app/lib/carillon_discord_commands.wasm
INSTALLED=$INSTALLATION/state/components/discord-private/command-module.wasm
carillon discord commands | grep -Fx 'discord-private serves the release command module: /model, /command.' >/dev/null
carillon discord commands check "$MODULE" | grep -F 'is a usable command module' >/dev/null
# The running provider takes the installation's maintenance lock for its own changes.
refuse locked-install flock "$INSTALLATION/.maintenance.lock" carillon discord commands install "$MODULE"
grep -F 'another Carillon maintenance operation is running' "$TEST_ROOT/locked-install.log" >/dev/null
grep -F 'Still serving the release command module: /model, /command.' "$TEST_ROOT/locked-install.log" >/dev/null
[ ! -e "$INSTALLED" ]
carillon discord configure --guild-id 100 --bot-user-id 201 --user-id 200 --user-id 250 \
  --route 300:always:channel >/dev/null
grep -F '"250"' "$INSTALLATION/providers/discord-private.toml" >/dev/null
carillon restart >/dev/null
expect_trace '--user restart carillon.target'
carillon logs core | grep -F 'Carillon core ready' >/dev/null
carillon logs -n 1 | grep -F "==> $INSTALLATION/logs/discord.log <==" >/dev/null
refuse log-escape carillon logs ../credentials/operator
if grep -F "$(cat "$INSTALLATION/credentials/operator.token")" "$TEST_ROOT/log-escape.log"; then
  fail 'logs exposed a credential'
fi
# Maintenance is serialized; a held lock refuses before touching the services.
refuse locked flock "$INSTALLATION/.maintenance.lock" carillon stop
grep -F 'another Carillon maintenance operation is running' "$TEST_ROOT/locked.log" >/dev/null
[ ! -e "$TRACE" ]
running
printf 'operations passed: entry point, resolution, start/restart, logs, provider commands, command module, lock\n'

# An independently installed provider survives application replacement.
cat >"$HOME/.local/bin/carillon-review" <<'PROVIDER'
#!/bin/sh
printf 'review %s %s\n' "$CARILLON_HOME" "$*"
PROVIDER
chmod 0755 "$HOME/.local/bin/carillon-review"
printf '%s\n' owner-pi-state >"$INSTALLATION/pi/operator-sentinel"
NEXT=$(release_archive next)
pack "$NEXT" "$TEST_ROOT/next.tar.gz"
recent_before=$(carillon recent)
app_inode=$(stat -c %i "$INSTALLATION/app")

# Refused archives change nothing and leave the services running.
BAD=$(release_archive link)
ln -s /etc "$BAD/lib/link"
pack "$BAD" "$TEST_ROOT/link.tar.gz"
BAD=$(release_archive relabelled)
mv "$BAD" "${BAD%-"$VERSION"}-9.9.9"
pack "${BAD%-"$VERSION"}-9.9.9" "$TEST_ROOT/relabelled.tar.gz"
BAD=$(release_archive broken)
printf '#!/bin/sh\nexit 1\n' >"$BAD/bin/carillon"
pack "$BAD" "$TEST_ROOT/broken.tar.gz"
mkdir -p "$TEST_ROOT/traversal/inner"
printf '%s\n' escaped >"$TEST_ROOT/traversal/escape"
(cd "$TEST_ROOT/traversal/inner" && tar -P -czf ../../traversal.tar.gz ../escape)
printf '%s\n' 'not an archive' >"$TEST_ROOT/text.tar.gz"
# Incomplete or altered payloads are refused against the packager's MANIFEST.
BAD=$(release_archive missing-provider)
rm "$BAD/bin/carillon-discord"
pack "$BAD" "$TEST_ROOT/missing-provider.tar.gz"
BAD=$(release_archive nonexec-installer)
chmod 0644 "$BAD/install.sh"
pack "$BAD" "$TEST_ROOT/nonexec-installer.tar.gz"
BAD=$(release_archive nonexec-provider)
chmod 0644 "$BAD/bin/carillon-discord"
pack "$BAD" "$TEST_ROOT/nonexec-provider.tar.gz"
BAD=$(release_archive unlisted)
printf '%s\n' unlisted >"$BAD/bin/unlisted"
pack "$BAD" "$TEST_ROOT/unlisted.tar.gz"
snapshot
for archive in link:'unsupported entry' relabelled:'not version 9.9.9' broken:'reports ""' \
  traversal:'unsafe path' text:'nothing was changed' \
  missing-provider:'release is incomplete: bin/carillon-discord is missing' \
  nonexec-installer:'install.sh is missing or not executable' \
  nonexec-provider:'release file bin/carillon-discord is not executable' \
  unlisted:'files its MANIFEST does not list'; do
  refuse "refused-${archive%%:*}" carillon update "$TEST_ROOT/${archive%%:*}.tar.gz"
  grep -F "${archive#*:}" "$TEST_ROOT/refused-${archive%%:*}.log" >/dev/null
  grep -F 'nothing was changed' "$TEST_ROOT/refused-${archive%%:*}.log" >/dev/null
  expect_trace '--user show --property=ActiveState --value carillon.target'
  [ "$(stat -c %i "$INSTALLATION/app")" = "$app_inode" ]
  running
  [ ! -e "$TEST_ROOT/escape" ]
  [ ! -e "$INSTALLATION/escape" ]
  assert_preserved
done
printf 'update refusals passed: links, traversal, relabelled, unrunnable and incomplete releases\n'

# The release's own programs refuse installed state they cannot use, before
# anything is stopped: a core schema this release does not know, and Discord
# settings its provider rejects.
DATABASE=$INSTALLATION/state/carillon.sqlite3
DISCORD_SETTINGS=$INSTALLATION/providers/discord-private.toml
cp -p "$DISCORD_SETTINGS" "$TEST_ROOT/discord-settings"
for state in schema:'database schema is not the current Carillon format' \
  discord:'cannot use the settings of discord-private' \
  module:'has an installed command module this release cannot serve'; do
  case "${state%%:*}" in
    schema)
      node -e 'new (require("node:sqlite").DatabaseSync)(process.argv[1]).exec("CREATE TABLE later_release (id INTEGER)")' \
        "$DATABASE" ;;
    discord) sed -i 's/^guild_id = .*/guild_id = "not-a-snowflake"/' "$DISCORD_SETTINGS" ;;
    # An installed module the new release cannot serve, as after a contract change.
    module) mkdir -p "${INSTALLED%/*}" && printf '%s\n' 'not a module' >"$INSTALLED" ;;
  esac
  snapshot
  refuse "incompatible-${state%%:*}" carillon update "$TEST_ROOT/next.tar.gz"
  grep -F "${state#*:}" "$TEST_ROOT/incompatible-${state%%:*}.log" >/dev/null
  grep -F 'nothing was changed' "$TEST_ROOT/incompatible-${state%%:*}.log" >/dev/null
  expect_trace '--user show --property=ActiveState --value carillon.target'
  [ "$(stat -c %i "$INSTALLATION/app")" = "$app_inode" ]
  running
  assert_preserved
  case "${state%%:*}" in
    schema)
      node -e 'new (require("node:sqlite").DatabaseSync)(process.argv[1]).exec("DROP TABLE later_release")' \
        "$DATABASE" ;;
    discord) cp -p "$TEST_ROOT/discord-settings" "$DISCORD_SETTINGS" ;;
    module) rm "$INSTALLED" ;;
  esac
done
snapshot
printf 'update state preflight passed: incompatible core schema, provider settings and command module refused\n'

# An installer that cannot even be started after the services stopped leaves
# the application unchanged and says how to resume it.
refuse unrunnable-installer env FAKE_STOP=break-installer carillon update "$TEST_ROOT/next.tar.gz"
grep -F 'could not run the release installer, so the application was not replaced' \
  "$TEST_ROOT/unrunnable-installer.log" >/dev/null
grep -F "Services are stopped (they were running); \`carillon start\` resumes the current release" \
  "$TEST_ROOT/unrunnable-installer.log" >/dev/null
[ "$(stat -c %i "$INSTALLATION/app")" = "$app_inode" ]
stopped
assert_preserved
carillon start >/dev/null
rm -f "$TRACE"

# A running installation is stopped, replaced, restarted and checked. An
# installed command module lives outside the replaced application and is
# checked by the new release before anything stops.
carillon discord commands install "$MODULE" >"$TEST_ROOT/commands-install.log"
grep -Fx 'discord-private now serves the installed command module: /model, /command.' \
  "$TEST_ROOT/commands-install.log" >/dev/null
cmp "$MODULE" "$INSTALLED"
carillon update "$TEST_ROOT/next.tar.gz" >"$TEST_ROOT/update.log"
grep -F "installed command module $INSTALLED is usable" "$TEST_ROOT/update.log" >/dev/null
grep -Fx "Updated to Carillon $VERSION; services are running again:" "$TEST_ROOT/update.log" >/dev/null
expect_trace '--user show --property=ActiveState --value carillon.target' \
  '--user stop carillon.target' '--user daemon-reload' '--user enable carillon.target' \
  '--user start carillon.target'
[ "$(cat "$INSTALLATION/app/lib/release-sentinel")" = next ]
[ "$(readlink "$ENTRY")" = "$INSTALLATION/app/bin/carillon" ]
assert_preserved
[ -z "$(find "$INSTALLATION" -maxdepth 1 -name '.app-new.*')" ]
[ "$(carillon recent)" = "$recent_before" ]
case "$(simulate installed-2)" in *'Local provider response 2'*) ;; *) fail 'conversation did not continue' ;; esac
[ "$(carillon review check)" = "review $INSTALLATION check" ]
carillon discord commands | grep -Fx 'discord-private serves the installed command module: /model, /command.' >/dev/null
carillon discord commands reset | grep -Fx 'discord-private now serves the release command module: /model, /command.' >/dev/null
[ ! -e "$INSTALLED" ]

# A stopped installation stays stopped.
carillon stop >/dev/null
rm -f "$TRACE"
FINAL=$(release_archive final)
pack "$FINAL" "$TEST_ROOT/final.tar.gz"
carillon update "$TEST_ROOT/final.tar.gz" | grep -F 'services remain stopped as before' >/dev/null
expect_trace '--user show --property=ActiveState --value carillon.target' \
  '--user daemon-reload' '--user enable carillon.target'
[ "$(cat "$INSTALLATION/app/lib/release-sentinel")" = final ]
stopped
assert_preserved
printf 'update passed: running and stopped states restored, configuration and state preserved\n'

# Registration failure after application exchange: say what changed, keep the
# services stopped, and finish with a rerun.
carillon start >/dev/null
rm -f "$TRACE"
refuse publish-failure env FAKE_RELOAD=fail carillon update "$TEST_ROOT/next.tar.gz"
grep -F 'Services are stopped (they were running)' "$TEST_ROOT/publish-failure.log" >/dev/null
grep -F "Rerun \`carillon update $TEST_ROOT/next.tar.gz\`" "$TEST_ROOT/publish-failure.log" >/dev/null
stopped
[ "$(cat "$INSTALLATION/app/lib/release-sentinel")" = next ]
assert_preserved
# The previous application is retained for inspection, not adopted by the rerun.
[ -n "$(find "$INSTALLATION" -maxdepth 1 -name '.app-new.*')" ]
carillon update "$TEST_ROOT/next.tar.gz" | grep -F 'services remain stopped as before' >/dev/null
# Start and health failures leave the new release in place with recovery advice.
carillon start >/dev/null
rm -f "$TRACE"
refuse start-failure env FAKE_START=fail carillon update "$TEST_ROOT/final.tar.gz"
grep -F 'restarting the services failed' "$TEST_ROOT/start-failure.log" >/dev/null
grep -F 'the previous application was not kept' "$TEST_ROOT/start-failure.log" >/dev/null
[ "$(cat "$INSTALLATION/app/lib/release-sentinel")" = final ]
stopped
carillon start >/dev/null
rm -f "$TRACE"
refuse unhealthy env FAKE_START=no-discord carillon update --timeout 3 "$TEST_ROOT/next.tar.gz"
grep -F 'services started but are not healthy' "$TEST_ROOT/unhealthy.log" >/dev/null
grep -F 'unavailable: discord-private' "$TEST_ROOT/unhealthy.log" >/dev/null
carillon restart >/dev/null
assert_preserved
case "$(simulate installed-3)" in *'Local provider response 3'*) ;; *) fail 'conversation did not recover' ;; esac
carillon stop >/dev/null
rm -f "$TRACE"
printf 'update failures passed: registration, start and health report state and recovery\n'

# A second installation neither takes the entry point nor lets the first
# installation's commands control the units it now owns.
install_release "$ALT_PORT" CARILLON_HOME="$ALTERNATE" >"$TEST_ROOT/alternate.log"
grep -F "$ENTRY runs the installation at $INSTALLATION; left unchanged." "$TEST_ROOT/alternate.log" >/dev/null
grep -F "Start deliberately with: $ALTERNATE/app/bin/carillon start" "$TEST_ROOT/alternate.log" >/dev/null
[ "$(readlink "$ENTRY")" = "$INSTALLATION/app/bin/carillon" ]
rm -f "$TRACE"
refuse other-start carillon start
grep -F "units run the installation at $ALTERNATE, not $INSTALLATION" "$TEST_ROOT/other-start.log" >/dev/null
app_inode=$(stat -c %i "$INSTALLATION/app")
refuse other-update carillon update "$TEST_ROOT/next.tar.gz"
grep -F 'would take them over' "$TEST_ROOT/other-update.log" >/dev/null
[ "$(stat -c %i "$INSTALLATION/app")" = "$app_inode" ]
[ ! -e "$TRACE" ]
"$ALTERNATE/app/bin/carillon" start >/dev/null
carillon --home "$ALTERNATE" status | grep -F "\"version\": \"$VERSION\"" >/dev/null
CARILLON_HOME=$ALTERNATE carillon status >/dev/null
refuse default-status carillon status
"$ALTERNATE/app/bin/carillon" stop >/dev/null
"$INSTALLATION/app/bin/carillon-register-services" >/dev/null
carillon start >/dev/null
carillon stop >/dev/null
printf 'alternate installation passed: entry point kept, unit ownership enforced\n'

# Existing unrelated executables are never replaced; stale Carillon links are.
for existing in unrelated stale; do
  owner=$TEST_ROOT/$existing-owner
  mkdir -p "$owner/workspace" "$owner/.local/bin"
  if [ "$existing" = unrelated ]; then
    printf '#!/bin/sh\necho unrelated\n' >"$owner/.local/bin/carillon"
  else
    ln -s "$TEST_ROOT/removed/app/bin/carillon" "$owner/.local/bin/carillon"
  fi
  env HOME="$owner" XDG_CONFIG_HOME="$owner/.config" CARILLON_SKIP_SYSTEMD_RELOAD=1 \
    "$RELEASE/install.sh" --guild-id 100 --bot-user-id 201 --user-id 200 \
    --route 300:always:channel --workspace "$owner/workspace" --base-port "$ALT_PORT" \
    >"$TEST_ROOT/$existing.log"
  if [ "$existing" = unrelated ]; then
    [ "$(cat "$owner/.local/bin/carillon")" = "$(printf '#!/bin/sh\necho unrelated')" ]
    grep -F 'is not a Carillon entry point; left unchanged.' "$TEST_ROOT/$existing.log" >/dev/null
  else
    [ "$(readlink "$owner/.local/bin/carillon")" = "$owner/.carillon/app/bin/carillon" ]
  fi
done

# Uninstalling the alternate keeps the default's entry point and the units
# registered for it; the default then removes its own.
rm -f "$TRACE"
"$ALTERNATE/app/bin/carillon-uninstall" >/dev/null
[ "$(readlink "$ENTRY")" = "$INSTALLATION/app/bin/carillon" ]
grep -Fx "Environment=\"CARILLON_HOME=$INSTALLATION\"" "$XDG_CONFIG_HOME/systemd/user/carillon-core.service" >/dev/null
expect_trace '--user daemon-reload'
"$INSTALLATION/app/bin/carillon-uninstall" >/dev/null
expect_trace '--user disable --now carillon.target' '--user daemon-reload'
[ ! -e "$XDG_CONFIG_HOME/systemd/user/carillon-core.service" ]
[ ! -L "$ENTRY" ]
[ -x "$HOME/.local/bin/carillon-review" ]
printf 'entry point passed: unrelated kept, stale replaced, removed only by its installation\n'
