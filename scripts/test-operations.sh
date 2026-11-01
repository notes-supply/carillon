#!/bin/sh
set -eu

# Log assertions below require the normal startup messages, regardless of the
# invoking shell's logging filter. This affects only the test's child services.
export RUST_LOG=carillon=info

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
      --module "$root/app/lib/carillon_discord_module.wasm" --deterministic \
      >>"$root/logs/discord.log" 2>&1 &
    echo "$!" >>"$MANAGER/pids"
  fi
}
stop() {
  [ "${FAKE_STOP:-ok}" != fail ] || exit 1
  if [ "${FAKE_STOP:-ok}" = hold ]; then
    # Pause inside the service manager, then record which installation the
    # units named when the stop acted.
    : >"$MANAGER/holding"
    waited=0
    while [ ! -e "$MANAGER/release" ] && [ "$waited" -lt 400 ]; do
      waited=$((waited + 1))
      sleep 0.05
    done
    sed -n 's/^Environment="CARILLON_HOME=\(.*\)"$/\1/p' "$units/carillon-core.service" >"$MANAGER/stopped"
  fi
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
  # Whether the target or any of its services runs, as setup asks; with
  # FAKE_MEMBER, a service in that state follows the target's.
  '--user show --property=ActiveState --value carillon.target carillon-core.service carillon-discord.service carillon-runtime-pi.service carillon-runtime-t3.service')
    if running; then echo active; else echo inactive; fi
    [ -z "${FAKE_MEMBER:-}" ] || printf '\n%s\n' "$FAKE_MEMBER" ;;
  # The services that are part of the target, which a stop names so that it
  # returns once they, not only the target, have stopped.
  '--user show --property=ConsistsOf --value carillon.target')
    echo 'carillon-core.service carillon-discord.service carillon-runtime-pi.service carillon-runtime-t3.service' ;;
  '--user start carillon.target') running || start ;;
  '--user stop carillon.target carillon-core.service carillon-discord.service carillon-runtime-pi.service carillon-runtime-t3.service') stop ;;
  # The test's own cleanup stops whatever is left.
  '--user stop carillon.target') stop ;;
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
    for log in "$TEST_ROOT"/*.log "$INSTALLATION"/logs/*.log "$ALTERNATE"/logs/*.log; do
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
    --route 300:always:channel:assistant --workspace "$HOME/workspace" --base-port "$port"
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
grep -Fx "  discord-private: ready ($VERSION)" "$TEST_ROOT/start.log" >/dev/null
expect_trace '--user start carillon.target'
# Resolution never consults the current directory, even one that looks like an installation.
mkdir "$TEST_ROOT/decoy"
printf '%s\n' 'not a registry' >"$TEST_ROOT/decoy/carillon.toml"
(cd "$TEST_ROOT/decoy" && carillon status) | grep -F '"status": "ready"' >/dev/null
case "$(simulate installed-1)" in *'Local provider response 1'*) ;; *) fail 'first turn failed' ;; esac

# Provider commands receive the resolved installation and infer the only Discord instance.
EXAMPLE=$INSTALLATION/app/lib/examples/card-colours.wasm
SETTINGS=$INSTALLATION/providers/discord-private.toml
carillon discord module | grep -Fx 'discord-private serves the bundled Discord module: /settings, /command.' >/dev/null
carillon discord module check "$EXAMPLE" | grep -F 'is a usable Discord module' >/dev/null
# Another maintenance operation's lock leaves the declaration as it is.
cp "$SETTINGS" "$TEST_ROOT/settings-before"
refuse locked-enable flock "$INSTALLATION/.maintenance.lock" carillon discord module enable "$EXAMPLE"
grep -F 'another Carillon maintenance operation is running' "$TEST_ROOT/locked-enable.log" >/dev/null
cmp "$SETTINGS" "$TEST_ROOT/settings-before"
carillon discord module | grep -Fx 'discord-private serves the bundled Discord module: /settings, /command.' >/dev/null
carillon discord configure --guild-id 100 --bot-user-id 201 --user-id 200 --user-id 250 \
  --route 300:always:channel:assistant >/dev/null
grep -F '"250"' "$INSTALLATION/providers/discord-private.toml" >/dev/null
# A restart drains core first, then stops the target with its services.
carillon restart >"$TEST_ROOT/restart.log"
grep -Fx 'Core stopped with its runtime workers paused.' "$TEST_ROOT/restart.log" >/dev/null
expect_trace '--user show --property=ActiveState --value carillon.target' \
  '--user show --property=ConsistsOf --value carillon.target' \
  '--user stop carillon.target carillon-core.service carillon-discord.service carillon-runtime-pi.service carillon-runtime-t3.service' \
  '--user start carillon.target'
grep -F 'runtime workers paused for maintenance' "$INSTALLATION/logs/core.log" >/dev/null
# A runtime that is not running holds nothing a restart loses, as one that
# is disabled or serves no agent does not run: the restart goes ahead and
# starts it again.
kill "$(head -n 1 "$MANAGER/pids")"
carillon restart >"$TEST_ROOT/restart-exited.log"
grep -Fx 'Core stopped with its runtime workers paused.' "$TEST_ROOT/restart-exited.log" >/dev/null
expect_trace '--user show --property=ActiveState --value carillon.target' \
  '--user show --property=ConsistsOf --value carillon.target' \
  '--user stop carillon.target carillon-core.service carillon-discord.service carillon-runtime-pi.service carillon-runtime-t3.service' \
  '--user start carillon.target'
# A runtime that cannot say whether a restart loses anything keeps the
# services running and stops nothing. Refused when its time is up, the
# command asks nothing more of core: the pause lapses within its lease and
# the workers resume. Forced, the restart goes ahead and says what it
# interrupts.
frozen=$(head -n 1 "$MANAGER/pids")
kill -STOP "$frozen"
refuse undrained carillon restart --timeout 10
grep -F 'core did not drain within 10 seconds' "$TEST_ROOT/undrained.log" >/dev/null
grep -F 'runtime-pi is not ready to restart: it did not say whether it can restart' \
  "$TEST_ROOT/undrained.log" >/dev/null
expect_trace '--user show --property=ActiveState --value carillon.target'
attempt=0
until carillon registry >"$TEST_ROOT/resumed.log" &&
  ! grep -F 'Maintenance holds the workers paused' "$TEST_ROOT/resumed.log" >/dev/null; do
  attempt=$((attempt + 1))
  [ "$attempt" -lt 120 ] || fail 'the workers did not resume when the pause lapsed'
  sleep 0.25
done
grep -F 'runtime-pi: enabled; its dispatcher hands it its turns' \
  "$TEST_ROOT/resumed.log" >/dev/null
carillon status | grep -F '"status": "unavailable"' >/dev/null
kill -CONT "$frozen"
# The forced restart waits, as any restart does, until every provider answers.
carillon restart --force >"$TEST_ROOT/forced.log"
grep -F 'Forcing the stop before this settles' "$TEST_ROOT/forced.log" >/dev/null
grep -F 'runtime-pi: ready' "$TEST_ROOT/forced.log" >/dev/null
rm -f "$TRACE"
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
printf 'operations passed: entry point, resolution, start/drained restart, logs, provider commands, Discord module, lock\n'

# Setup over this installation while it runs would leave its services running
# the replaced release, so it is refused before it asks or writes anything:
# with options or guided, with registration's reload skipped, and while only
# one of its services runs.
ACTIVE='--user show --property=ActiveState --value carillon.target carillon-core.service carillon-discord.service carillon-runtime-pi.service carillon-runtime-t3.service'
app_inode=$(stat -c %i "$INSTALLATION/app")
snapshot
for setup in options guided skip-reload member; do
  case "$setup" in
    options) refuse "running-setup-$setup" install_release "$BASE_PORT" ;;
    guided) refuse "running-setup-$setup" "$RELEASE/install.sh" ;;
    skip-reload)
      refuse "running-setup-$setup" install_release "$BASE_PORT" CARILLON_SKIP_SYSTEMD_RELOAD=1 ;;
    member)
      carillon stop >/dev/null
      rm -f "$TRACE"
      refuse "running-setup-$setup" install_release "$BASE_PORT" FAKE_MEMBER=activating ;;
  esac
  grep -F "Carillon at $INSTALLATION is running" "$TEST_ROOT/running-setup-$setup.log" >/dev/null
  grep -F "Update it with 'carillon update /path/to/carillon-VERSION-linux-x86_64.tar.gz'" \
    "$TEST_ROOT/running-setup-$setup.log" >/dev/null
  expect_trace "$ACTIVE"
  [ "$(stat -c %i "$INSTALLATION/app")" = "$app_inode" ]
  [ "$setup" = member ] || running
  assert_preserved
done
stopped
carillon start >/dev/null
rm -f "$TRACE"
running
printf 'running setup refused: with options, guided, reload skipped, and only a service running\n'

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
# anything is stopped: a core schema this release does not know, the
# previous release's, which records no conversation's channel, and Discord
# settings its provider rejects.
DATABASE=$INSTALLATION/state/carillon.sqlite3
DISCORD_SETTINGS=$INSTALLATION/providers/discord-private.toml
cp -p "$DISCORD_SETTINGS" "$TEST_ROOT/discord-settings"
for state in schema:'database schema is not the current Carillon format' \
  previous:'database schema is not the current Carillon format' \
  discord:'cannot use the settings of discord-private' \
  module:'enable a custom Discord module this release cannot serve'; do
  case "${state%%:*}" in
    schema)
      node -e 'new (require("node:sqlite").DatabaseSync)(process.argv[1]).exec("CREATE TABLE later_release (id INTEGER)")' \
        "$DATABASE" ;;
    # The table is kept aside, and the write-ahead log emptied into the
    # database, so its digest shows whether the refused update wrote.
    previous)
      node -e '
        const database = new (require("node:sqlite").DatabaseSync)(process.argv[1]);
        const sql = database.prepare("SELECT sql FROM sqlite_schema WHERE name = ?").get("conversations").sql;
        const rows = database.prepare("SELECT * FROM conversations").all();
        require("node:fs").writeFileSync(process.argv[2], JSON.stringify({ sql, rows }));
        database.exec("DROP TABLE conversations; PRAGMA wal_checkpoint(TRUNCATE)");
        database.close();' "$DATABASE" "$TEST_ROOT/conversations.json"
      sha256sum "$DATABASE" >"$TEST_ROOT/database.sha256" ;;
    discord) sed -i 's/^guild_id = .*/guild_id = "not-a-snowflake"/' "$DISCORD_SETTINGS" ;;
    # An enabled module the new release cannot serve, as after a contract change.
    module)
      printf '%s\n' 'not a module' >"$TEST_ROOT/broken.wasm"
      printf '\n[module]\nenabled = true\npath = "%s"\n' "$TEST_ROOT/broken.wasm" >>"$DISCORD_SETTINGS" ;;
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
    previous)
      sha256sum --check --status "$TEST_ROOT/database.sha256" || fail 'a refused update changed the database'
      node -e '
        const database = new (require("node:sqlite").DatabaseSync)(process.argv[1]);
        const { sql, rows } = JSON.parse(require("node:fs").readFileSync(process.argv[2], "utf8"));
        database.exec(sql);
        const insert = database.prepare(
          "INSERT INTO conversations (interface_id, agent_id, audience_id, channel_id) VALUES (?, ?, ?, ?)");
        for (const row of rows) insert.run(row.interface_id, row.agent_id, row.audience_id, row.channel_id);
        database.close();' "$DATABASE" "$TEST_ROOT/conversations.json" ;;
    discord) cp -p "$TEST_ROOT/discord-settings" "$DISCORD_SETTINGS" ;;
    module) cp -p "$TEST_ROOT/discord-settings" "$DISCORD_SETTINGS" && rm "$TEST_ROOT/broken.wasm" ;;
  esac
done
snapshot
printf 'update state preflight passed: incompatible and previous core schemas, provider settings and Discord module refused\n'

# When T3 is the registered runtime, the release's own T3 runtime reads the
# settings and token it would start from before anything stops. A token file
# gone or emptied while the running runtime still holds its token, or
# settings it rejects, are refused, as above.
cp -p "$INSTALLATION/carillon.toml" "$TEST_ROOT/registry"
# The assistant becomes plain T3 Code, which takes no resources.
sed -i -e 's/"runtime-pi"/"runtime-t3"/' -e '/^resources = /d' "$INSTALLATION/carillon.toml"
T3_SETTINGS=$INSTALLATION/providers/runtime-t3.toml
T3_TOKEN=$INSTALLATION/credentials/t3.token
t3_settings() {
  printf '%s\n' 'url = "http://127.0.0.1:1"' 'token_file = "credentials/t3.token"' \
    '[agents.assistant]' 'project_id = "agent"' >"$T3_SETTINGS"
  printf '%s\n' t3-token >"$T3_TOKEN"
  chmod 0600 "$T3_SETTINGS" "$T3_TOKEN"
}
for state in empty:'the T3 token file is empty' missing:'credentials/t3.token' \
  settings:'runtime-t3.toml: /agents/assistant must not have additional properties: model'; do
  t3_settings
  case "${state%%:*}" in
    empty) : >"$T3_TOKEN" ;;
    missing) rm "$T3_TOKEN" ;;
    # An earlier release's settings, which named the agent's model.
    settings)
      printf '%s\n' '[agents.assistant.model]' 'instance_id = "claudeAgent"' 'model = "claude-sonnet-5"' \
        >>"$T3_SETTINGS" ;;
  esac
  snapshot
  refuse "incompatible-t3-${state%%:*}" carillon update "$TEST_ROOT/next.tar.gz"
  grep -F "${state#*:}" "$TEST_ROOT/incompatible-t3-${state%%:*}.log" >/dev/null
  grep -F 'nothing was changed' "$TEST_ROOT/incompatible-t3-${state%%:*}.log" >/dev/null
  expect_trace '--user show --property=ActiveState --value carillon.target'
  [ "$(stat -c %i "$INSTALLATION/app")" = "$app_inode" ]
  running
  assert_preserved
done
# Usable settings and token pass the release's preflight.
t3_settings
CARILLON_HOME=$INSTALLATION "$RELEASE/install.sh" --update --check >"$TEST_ROOT/t3-preflight.log"
grep -Fx "runtime-t3's saved settings and T3 token read as startup reads them." \
  "$TEST_ROOT/t3-preflight.log" >/dev/null
cp -p "$TEST_ROOT/registry" "$INSTALLATION/carillon.toml"
rm "$T3_SETTINGS" "$T3_TOKEN"
snapshot
printf 'update T3 preflight passed: missing or empty token and rejected settings refused, usable ones accepted\n'

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

# A running installation is stopped, replaced, restarted and checked. The
# custom module the settings enable is checked by the new release before
# anything stops.
carillon discord module enable "$EXAMPLE" >"$TEST_ROOT/module-enable.log"
grep -Fx "It serves the custom Discord module $EXAMPLE: /settings, /command." \
  "$TEST_ROOT/module-enable.log" >/dev/null
snapshot
carillon update "$TEST_ROOT/next.tar.gz" >"$TEST_ROOT/update.log"
# A Pi installation has no T3 runtime to check.
grep -Fx 'runtime-t3 is not in carillon.toml; nothing to check' "$TEST_ROOT/update.log" >/dev/null
grep -F "custom Discord module $EXAMPLE, as this release ships it, is usable" "$TEST_ROOT/update.log" >/dev/null
grep -Fx "Updated to Carillon $VERSION; services are running again:" "$TEST_ROOT/update.log" >/dev/null
expect_trace '--user show --property=ActiveState --value carillon.target' \
  '--user show --property=ConsistsOf --value carillon.target' \
  '--user stop carillon.target carillon-core.service carillon-discord.service carillon-runtime-pi.service carillon-runtime-t3.service' \
  '--user daemon-reload' '--user enable carillon.target' '--user start carillon.target'
grep -Fx 'Core stopped with its runtime workers paused.' "$TEST_ROOT/update.log" >/dev/null
[ "$(cat "$INSTALLATION/app/lib/release-sentinel")" = next ]
[ "$(readlink "$ENTRY")" = "$INSTALLATION/app/bin/carillon" ]
assert_preserved
[ -z "$(find "$INSTALLATION" -maxdepth 1 -name '.app-new.*')" ]
[ "$(carillon recent)" = "$recent_before" ]
case "$(simulate installed-2)" in *'Local provider response 2'*) ;; *) fail 'conversation did not continue' ;; esac
[ "$(carillon review check)" = "review $INSTALLATION check" ]
carillon discord module | grep -Fx "discord-private serves the custom Discord module $EXAMPLE: /settings, /command." >/dev/null
carillon discord module disable | grep -Fx 'It serves the bundled Discord module: /settings, /command.' >/dev/null
snapshot

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

# While one installation acts on the shared units, another cannot register
# over them, so the action stays with the installation that checked them.
carillon start >/dev/null
rm -f "$MANAGER/holding" "$MANAGER/release" "$MANAGER/stopped"
FAKE_STOP=hold carillon stop >"$TEST_ROOT/held-stop.log" 2>&1 &
held=$!
attempt=0
until [ -e "$MANAGER/holding" ]; do
  attempt=$((attempt + 1))
  [ "$attempt" -lt 200 ] || fail 'the stop never reached the service manager'
  sleep 0.05
done
switched=0
"$ALTERNATE/app/bin/carillon-register-services" >"$TEST_ROOT/switch-during-stop.log" 2>&1 ||
  switched=$?
: >"$MANAGER/release"
wait "$held"
[ "$(cat "$MANAGER/stopped")" = "$INSTALLATION" ] ||
  fail "the stop acted on the units of $(cat "$MANAGER/stopped")"
[ "$switched" -ne 0 ] || fail 'another installation registered over held units'
grep -F 'another Carillon operation is changing the user units' \
  "$TEST_ROOT/switch-during-stop.log" >/dev/null
grep -Fx "Environment=\"CARILLON_HOME=$INSTALLATION\"" \
  "$XDG_CONFIG_HOME/systemd/user/carillon-core.service" >/dev/null
# A failed operation releases the units too: the deliberate switch, and the
# switch back, then succeed.
refuse failed-stop env FAKE_STOP=fail carillon stop
"$ALTERNATE/app/bin/carillon-register-services" >/dev/null
grep -Fx "Environment=\"CARILLON_HOME=$ALTERNATE\"" \
  "$XDG_CONFIG_HOME/systemd/user/carillon-core.service" >/dev/null
"$INSTALLATION/app/bin/carillon-register-services" >/dev/null
printf 'unit ownership passed: no registration over held units, released after failure\n'

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
    --route 300:always:channel:assistant --workspace "$owner/workspace" --base-port "$ALT_PORT" \
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
