#!/bin/sh
set -eu

# Representative stopped-maintenance failures against the real packaged scripts.
# Recovery is a successful rerun, not automatic application/unit rollback.
RELEASE=$(realpath -e -- "${1:?release directory required}")
TEST_ROOT=$(mktemp -d)
cleanup() {
  result=$?
  if [ "$result" -ne 0 ] && [ -f "$TEST_ROOT/install.log" ]; then
    cat "$TEST_ROOT/install.log" >&2
  fi
  rm -rf -- "$TEST_ROOT"
  exit "$result"
}
trap cleanup EXIT
trap 'exit 1' HUP INT TERM
COMMANDS=$TEST_ROOT/commands
mkdir "$COMMANDS"
REAL_SED=$(command -v sed)
REAL_MV=$(command -v mv)
REAL_SYNC=$(command -v sync)
export REAL_SED REAL_MV REAL_SYNC
cat >"$COMMANDS/command" <<'COMMAND'
#!/bin/sh
set -eu
command=${0##*/}
last=
for argument do last=$argument; done
case "$command:$FAIL_MODE:$last" in
  sync:app-stage:"$CARILLON_HOME"/.app-new.* | sync:app-sync:"$CARILLON_HOME" | sync:unit-sync:"$UNIT_DIR")
    touch "$TRIGGERED"
    exit 1 ;;
  mv:app-before:"$CARILLON_HOME/app")
    touch "$TRIGGERED"
    exit 1 ;;
  mv:app-after:"$CARILLON_HOME/app")
    "$REAL_MV" "$@"
    touch "$TRIGGERED"
    exit 1 ;;
  sed:render:"$CARILLON_HOME/app/systemd/carillon.target")
    printf '%s\n' 'incomplete definition'
    touch "$TRIGGERED"
    exit 1 ;;
  mv:publish:"$UNIT_DIR/carillon.target")
    touch "$TRIGGERED"
    exit 1 ;;
  systemctl:reload:daemon-reload)
    touch "$TRIGGERED"
    exit 1 ;;
esac
case "$command" in
  sed) exec "$REAL_SED" "$@" ;;
  mv) exec "$REAL_MV" "$@" ;;
  sync) exec "$REAL_SYNC" "$@" ;;
  systemctl)
    # Setup asks whether the units it replaces run; these never ran, and only
    # changes are traced.
    case "$*" in
      '--user show --property=ActiveState --value '*) printf '%s\n' inactive; exit 0 ;;
    esac
    printf '%s\n' "$*" >>"$MANAGER_TRACE"
    case "$*" in
      '--user daemon-reload') ;;
      '--user enable carillon.target')
        mkdir -p "$UNIT_DIR/default.target.wants"
        ln -sf ../carillon.target "$UNIT_DIR/default.target.wants/carillon.target" ;;
      *) exit 2 ;;
    esac ;;
  *) exit 2 ;;
esac
COMMAND
chmod 0755 "$COMMANDS/command"
for command in sed mv sync systemctl; do ln -s command "$COMMANDS/$command"; done

install_release() {
  "$RELEASE/install.sh" --guild-id 100 --bot-user-id 201 --user-id 200 \
    --route 300:always:channel --workspace "$HOME/workspace" \
    --base-port "${CARILLON_TEST_BASE_PORT:-44800}" >"$TEST_ROOT/install.log" 2>&1
}

# Fresh publication and upgrade share code; only the absence of previous state
# needs separate cases. Do not multiply every failure by every fixture variant.
for scenario in existing:app-stage existing:app-before existing:app-after existing:app-sync \
  existing:render existing:publish existing:unit-sync existing:reload fresh:render fresh:publish; do
  HOME=$TEST_ROOT/${scenario%:*}-${scenario#*:}
  CARILLON_HOME=$HOME/installation
  XDG_CONFIG_HOME=$HOME/separate-config
  UNIT_DIR=$XDG_CONFIG_HOME/systemd/user
  TRIGGERED=$HOME/triggered
  MANAGER_TRACE=$HOME/manager-trace
  export HOME CARILLON_HOME XDG_CONFIG_HOME UNIT_DIR TRIGGERED MANAGER_TRACE
  mkdir -p "$HOME/workspace" "$UNIT_DIR/unrelated.service.d"
  chmod 0755 "$UNIT_DIR"
  printf '%s\n' unrelated >"$UNIT_DIR/unrelated.service"
  printf '%s\n' unrelated-drop-in >"$UNIT_DIR/unrelated.service.d/override.conf"
  CARILLON_SKIP_SYSTEMD_RELOAD=1
  export CARILLON_SKIP_SYSTEMD_RELOAD
  previous=${scenario%%:*}
  FAIL_MODE=none
  export FAIL_MODE
  if [ "$previous" = existing ]; then
    PATH="$COMMANDS:$PATH" install_release
    printf '%s\n' previous-application >"$CARILLON_HOME/app/upgrade-sentinel"
    old_inode=$(stat -c %i "$CARILLON_HOME/app")
    cp -a "$UNIT_DIR" "$HOME/expected-units"
    for unit in "$UNIT_DIR"/carillon*; do printf '%s\n' '# previous-unit' >>"$unit"; done
    chmod 0640 "$UNIT_DIR/carillon-core.service"
  else
    "$RELEASE/bin/carillon" --home "$CARILLON_HOME" init --workspace "$HOME/workspace" \
      --base-port "${CARILLON_TEST_BASE_PORT:-44800}" >/dev/null
  fi
  printf '%s\n' test-bot-token >"$CARILLON_HOME/credentials/discord-bot.token"
  chmod 0600 "$CARILLON_HOME/credentials/discord-bot.token"
  cp -a "$CARILLON_HOME/credentials" "$HOME/credentials-before"
  cp -a "$UNIT_DIR" "$HOME/units-before"
  cp "$CARILLON_HOME/carillon.toml" "$HOME/config-before"
  FAIL_MODE=${scenario#*:}
  export FAIL_MODE
  if PATH="$COMMANDS:$PATH" CARILLON_SKIP_SYSTEMD_RELOAD=0 install_release; then
    echo "installer unexpectedly succeeded: $scenario" >&2
    exit 1
  fi
  [ -f "$TRIGGERED" ]
  diff -r "$HOME/credentials-before" "$CARILLON_HOME/credentials"
  cmp "$HOME/config-before" "$CARILLON_HOME/carillon.toml"
  diff -r "$HOME/units-before/unrelated.service.d" "$UNIT_DIR/unrelated.service.d"
  cmp "$HOME/units-before/unrelated.service" "$UNIT_DIR/unrelated.service"
  [ "$(stat -c %a "$UNIT_DIR")" = 755 ]

  case "$FAIL_MODE" in
    app-stage | app-before)
      [ "$(stat -c %i "$CARILLON_HOME/app")" = "$old_inode" ]
      [ -f "$CARILLON_HOME/app/upgrade-sentinel" ] ;;
    *)
      diff -r --no-dereference "$RELEASE/bin" "$CARILLON_HOME/app/bin" --exclude=carillon-uninstall
      [ "$(stat -c %a "$CARILLON_HOME/app/bin/carillon")" = 755 ]
      [ ! -e "$CARILLON_HOME/app/upgrade-sentinel" ]
      if [ "$previous" = existing ]; then
        retained_app=$(find "$CARILLON_HOME" -maxdepth 1 -type d -name '.app-new.*')
        [ "$(stat -c %i "$retained_app")" = "$old_inode" ]
        [ -f "$retained_app/upgrade-sentinel" ]
        grep -F "retained tree $retained_app" "$TEST_ROOT/install.log" >/dev/null
      fi ;;
  esac
  case "$FAIL_MODE" in
    publish | unit-sync | reload)
      retained_units=$(find "$UNIT_DIR" -maxdepth 1 -type d -name '.carillon-units.*')
      [ -d "$retained_units/old" ]
      if [ "$previous" = existing ]; then
        for unit in "$HOME/units-before"/carillon*; do
          cmp "$unit" "$retained_units/old/${unit##*/}"
          [ "$(stat -c %a "$unit")" = "$(stat -c %a "$retained_units/old/${unit##*/}")" ]
          if [ "$FAIL_MODE:${unit##*/}" = publish:carillon.target ]; then
            cmp "$unit" "$UNIT_DIR/${unit##*/}"
          else
            cmp "$HOME/expected-units/${unit##*/}" "$UNIT_DIR/${unit##*/}"
            [ "$(stat -c %a "$UNIT_DIR/${unit##*/}")" = 600 ]
          fi
        done
      else
        [ ! -e "$UNIT_DIR/carillon.target" ]
        systemd-analyze verify "$UNIT_DIR"/carillon-*.service
      fi ;;
    # Registration leaves the owner's shared units lock, as locks persist.
    *) diff -r --no-dereference --exclude=.carillon-units.lock "$HOME/units-before" "$UNIT_DIR" ;;
  esac
  # No failure reaches enable/start; the substituted manager rejects any start.
  [ ! -e "$MANAGER_TRACE" ]
  FAIL_MODE=none
  PATH="$COMMANDS:$PATH" CARILLON_SKIP_SYSTEMD_RELOAD=0 install_release
  printf '%s\n' '--user daemon-reload' '--user enable carillon.target' >"$HOME/expected-manager"
  cmp "$HOME/expected-manager" "$MANAGER_TRACE"
  [ "$(readlink "$UNIT_DIR/default.target.wants/carillon.target")" = ../carillon.target ]
  systemd-analyze verify "$UNIT_DIR"/carillon.target "$UNIT_DIR"/carillon-*.service
  diff -r "$HOME/credentials-before" "$CARILLON_HOME/credentials"
  cmp "$HOME/config-before" "$CARILLON_HOME/carillon.toml"
  printf 'maintenance failure and retry passed: %s\n' "$scenario"
done
