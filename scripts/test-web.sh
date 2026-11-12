#!/bin/sh
set -eu

# The web chat in installed releases: its service leaves an installation
# without it alone, a fresh installation with it alone or beside Discord
# answers a message, and an existing installation adds it with one command.
# test-manager.sh stands in for the user service manager, so `carillon
# start` and `restart` run the packaged services as their units say.
RELEASE=$(realpath -e -- "${1:?release directory required}")
VERSION=${RELEASE##*/carillon-}
SCRIPTS=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
PORT=$((${CARILLON_TEST_BASE_PORT:-44800} + 56))
TEST_ROOT=$(mktemp -d)
COMMANDS=$TEST_ROOT/commands
MANAGER=$TEST_ROOT/manager
TRACE=$TEST_ROOT/systemctl.trace
REAL_PATH=$PATH
export MANAGER TRACE
unset CARILLON_HOME CARILLON_MAINTENANCE_LOCK CARILLON_SKIP_SYSTEMD_RELOAD
mkdir -p "$COMMANDS" "$MANAGER"
cp "$SCRIPTS/test-manager.sh" "$COMMANDS/systemctl"

cleanup() {
  result=$?
  systemctl --user stop carillon.target >/dev/null 2>&1 || true
  if [ "$result" -ne 0 ]; then
    for log in "$TEST_ROOT"/*.log "$HOME"/.carillon/logs/*.log; do
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

# owner NAME: a new owner, whose installation is the default `~/.carillon`.
owner() {
  HOME=$TEST_ROOT/$1
  XDG_CONFIG_HOME=$HOME/.config
  PATH=$HOME/.local/bin:$COMMANDS:$REAL_PATH
  INSTALLATION=$HOME/.carillon
  export HOME XDG_CONFIG_HOME PATH
  mkdir -p "$HOME/workspace"
}
# install_release [OPTION ...]: unattended, as the installer's options say.
install_release() {
  "$RELEASE/install.sh" "$@" --workspace "$HOME/workspace" --base-port "$PORT"
}
discord_options() {
  set -- --guild-id 100 --bot-user-id 201 --user-id 200 --route 300:always:channel:assistant
  printf '%s ' "$@"
}
# The registry's provider IDs in order; its agents follow them.
providers() {
  sed -n -e '/^\[\[agents\]\]/q' -e 's/^id = "\(.*\)"$/\1/p' "$INSTALLATION/carillon.toml" | tr '\n' ' '
}
chat() { "$SCRIPTS/test-web-chat.sh" "$1" "$TEST_ROOT/headers"; }
session() {
  curl --silent --output /dev/null --write-out '%{http_code}' -H "@$TEST_ROOT/headers" \
    "http://127.0.0.1:$((PORT + 4))/api/session"
}
uninstall() {
  "$INSTALLATION/app/bin/carillon-uninstall" >/dev/null
  [ -z "$(find "$XDG_CONFIG_HOME/systemd/user" -maxdepth 1 -name 'carillon*')" ]
  [ ! -e "$INSTALLATION/app" ]
}

# Its unit is in every installation's target. Where the registry has no web
# entry, or has it disabled, the program ends successfully before it reads
# its credential, so the service is not restarted.
owner absent
"$RELEASE/bin/carillon" --home "$INSTALLATION" init --interface discord \
  --workspace "$HOME/workspace" --base-port "$PORT" >/dev/null
serve() {
  RUST_LOG=info "$RELEASE/bin/carillon-web" serve --config "$INSTALLATION/carillon.toml" \
    --provider web --token-file "$INSTALLATION/missing.token" >"$TEST_ROOT/serve.log" 2>&1
}
serve
grep -F 'interface provider web is not in carillon.toml; not serving' "$TEST_ROOT/serve.log" >/dev/null
"$RELEASE/bin/carillon" --home "$INSTALLATION" init --interface web >/dev/null
"$RELEASE/bin/carillon" --home "$INSTALLATION" provider web disable >/dev/null
serve
grep -F 'interface provider web is disabled; not serving' "$TEST_ROOT/serve.log" >/dev/null
[ ! -e "$INSTALLATION/state/components/web/browser.key" ]
printf 'web service passed: it ends at once where it is not registered or is disabled\n'

# The web chat alone: no Discord entry, settings, token or question, and the
# Discord and T3 services of the target end at once.
owner alone
install_release --interface web >"$TEST_ROOT/alone.log"
grep -Fx '1. Sign in to a model provider with: '"$INSTALLATION"'/app/bin/carillon-pi --no-session --no-tools --no-approve' \
  "$TEST_ROOT/alone.log" >/dev/null
grep -Fx '3. Open the web chat in your browser with: carillon web open' "$TEST_ROOT/alone.log" >/dev/null
[ "$(providers)" = 'web runtime-pi ' ]
[ -z "$(find "$INSTALLATION" -path "$INSTALLATION/app" -prune -o -iname '*discord*' -print)" ] ||
  fail 'an installation of the web chat alone has something of Discord'
[ -z "$(ls "$INSTALLATION/providers")" ]
rm -f "$TRACE"
carillon start >"$TEST_ROOT/alone-start.log"
grep -Fx "  web: ready ($VERSION)" "$TEST_ROOT/alone-start.log" >/dev/null
! grep -F discord "$TEST_ROOT/alone-start.log" >/dev/null || fail 'core waits for Discord'
carillon settings --agent assistant model=local/echo | grep -Ex 'Model +Local echo' >/dev/null
chat 'hello from the page'
# A restart drains with the web chat registered and no browser reading: its
# answers are delivered once core records them.
carillon restart >"$TEST_ROOT/alone-restart.log"
grep -Fx 'Core stopped with its runtime workers paused.' "$TEST_ROOT/alone-restart.log" >/dev/null
grep -Fx "  web: ready ($VERSION)" "$TEST_ROOT/alone-restart.log" >/dev/null
[ "$(session)" = 200 ] || fail 'a restart signed the browser out'
chat 'after the restart'
carillon web sign-out | grep -Fx 'Every browser is signed out of Carillon'"'"'s web chat.' >/dev/null
[ "$(session)" = 401 ] || fail 'a signed-out browser was still let in'
carillon logs web | grep -F 'web interface provider ready' >/dev/null
carillon stop >/dev/null
uninstall
printf 'web chat alone passed: installed without Discord, answered, restarted drained, signed out, uninstalled\n'

# Beside Discord, each interface answers its own people.
owner both
# shellcheck disable=SC2046
install_release --interface web $(discord_options) >"$TEST_ROOT/both.log"
grep -Fx '1. Put the Discord bot token in '"$INSTALLATION"'/credentials/discord-bot.token (mode 0600).' \
  "$TEST_ROOT/both.log" >/dev/null
grep -Fx '4. Open the web chat in your browser with: carillon web open' "$TEST_ROOT/both.log" >/dev/null
[ "$(providers)" = 'web discord-private runtime-pi ' ]
carillon start >"$TEST_ROOT/both-start.log"
for id in web discord-private; do
  grep -Fx "  $id: ready ($VERSION)" "$TEST_ROOT/both-start.log" >/dev/null
done
carillon settings --agent assistant model=local/echo >/dev/null
chat 'hello from the page'
printf 'Authorization: Bearer %s\n' "$(cat "$INSTALLATION/credentials/discord-private.token")" \
  >"$TEST_ROOT/discord-headers"
curl --fail --silent -X POST -H 'content-type: application/json' -H "@$TEST_ROOT/discord-headers" \
  --data '{"event_id":"both-1","guild_id":"100","user_id":"200","channel_id":"300","content":"hello from Discord"}' \
  "http://127.0.0.1:$((PORT + 1))/v1/simulate" | grep -F 'Local provider response 1' >/dev/null
carillon stop >/dev/null
uninstall
printf 'web chat beside Discord passed: both registered, both answered\n'

# An installation that has Discord adds the web chat with one command, which
# asks nothing. Stopped, it stays stopped and says how to start.
owner added
# shellcheck disable=SC2046
install_release $(discord_options) >/dev/null
[ "$(providers)" = 'discord-private runtime-pi ' ]
cp -p "$INSTALLATION/carillon.toml" "$TEST_ROOT/registry"
cp -p "$INSTALLATION/providers/discord-private.toml" "$TEST_ROOT/discord-settings"
rm -f "$TRACE"
carillon setup --interface web </dev/null >"$TEST_ROOT/added.log" 2>&1
grep -F "Carillon is not running. \`carillon start\` starts it with web." "$TEST_ROOT/added.log" >/dev/null
[ ! -e "$TRACE" ] || fail 'adding an interface to a stopped installation reached the service manager'
# The registry gains the entry and loses nothing.
[ "$(providers)" = 'discord-private runtime-pi web ' ]
diff "$TEST_ROOT/registry" "$INSTALLATION/carillon.toml" >"$TEST_ROOT/registry.diff" || true
! grep '^<' "$TEST_ROOT/registry.diff" >/dev/null || fail 'adding the web chat changed another entry'
grep -Fx '> delivery = "retained"' "$TEST_ROOT/registry.diff" >/dev/null
grep -Fx '> max_upload_mib = 1024' "$TEST_ROOT/registry.diff" >/dev/null
cmp "$TEST_ROOT/discord-settings" "$INSTALLATION/providers/discord-private.toml"
carillon start >"$TEST_ROOT/added-start.log"
grep -Fx "  web: ready ($VERSION)" "$TEST_ROOT/added-start.log" >/dev/null
carillon settings --agent assistant model=local/echo >/dev/null
chat 'hello from the page'
# Again, with the web chat served: nothing changes and nothing restarts.
cp -p "$INSTALLATION/carillon.toml" "$TEST_ROOT/registry"
rm -f "$TRACE"
carillon setup --interface web </dev/null >"$TEST_ROOT/again.log" 2>&1
grep -Fx 'Carillon already serves web, so it was not restarted.' "$TEST_ROOT/again.log" >/dev/null
cmp "$TEST_ROOT/registry" "$INSTALLATION/carillon.toml"
[ ! -e "$TRACE" ] || fail 'setting up a served interface again reached the service manager'
[ "$(session)" = 200 ]
carillon stop >/dev/null
uninstall
printf 'web chat added passed: one command on a stopped installation, nothing else changed, a rerun changes nothing\n'
