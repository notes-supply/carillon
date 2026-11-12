#!/bin/sh
set -eu

# A stand-in for `systemctl --user` in the installation tests. It runs the
# services of the registered `carillon.target` as their units say, with
# deterministic Discord and Pi, so the packaged programs start, answer and
# stop as an installation's do. It expects MANAGER, a directory for its
# state, and TRACE, the file its calls are appended to. The FAKE_ variables
# make one call fail or pause.
printf '%s\n' "$*" >>"$TRACE"
units=$XDG_CONFIG_HOME/systemd/user
# Empty once the units are removed, when only a reload is still asked for.
services=$(sed -n 's/^Wants=//p' "$units/carillon.target" 2>/dev/null) || services=
root=$(sed -n 's/^Environment="CARILLON_HOME=\(.*\)"$/\1/p' "$units/carillon-core.service" 2>/dev/null) ||
  root=
running() { [ -s "$MANAGER/pids" ]; }
start() {
  [ "${FAKE_START:-ok}" != fail ] || exit 1
  for unit in $services; do
    command=$(sed -n 's/^ExecStart=//p' "$units/$unit")
    case "$unit" in
      carillon-discord.service)
        [ "${FAKE_START:-ok}" != no-discord ] || continue
        command="$command --deterministic" ;;
      carillon-runtime-pi.service) command="$command --deterministic" ;;
    esac
    # A test path needs none of systemd's escapes, so a unit's quoting is the shell's.
    env CARILLON_HOME="$root" PI_CODING_AGENT_DIR="$root/pi" sh -c "exec $command" \
      >>"$(sed -n 's/^StandardOutput=append://p' "$units/$unit")" 2>&1 &
    echo "$!" >>"$MANAGER/pids"
  done
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
    printf '%s\n' "$root" >"$MANAGER/stopped"
  fi
  # Make the already checked installer unrunnable once services are down.
  [ "${FAKE_STOP:-ok}" != break-installer ] || chmod a-x "$root"/.update-*/carillon-*/install.sh
  running || return 0
  while read -r pid; do kill "$pid" 2>/dev/null || true; done <"$MANAGER/pids"
  while read -r pid; do
    while kill -0 "$pid" 2>/dev/null; do sleep 0.05; done
  done <"$MANAGER/pids"
  rm -f "$MANAGER/pids"
}
case "$*" in
  '--user show --property=ActiveState --value carillon.target')
    if running; then echo active; else echo inactive; fi ;;
  # Whether the target or any of its services runs, as setup asks; with
  # FAKE_MEMBER, a service in that state follows the target's.
  '--user show --property=ActiveState --value '*' carillon.target')
    if running; then echo active; else echo inactive; fi
    [ -z "${FAKE_MEMBER:-}" ] || printf '\n%s\n' "$FAKE_MEMBER" ;;
  # The services that are part of the target, which a stop names so that it
  # returns once they, not only the target, have stopped.
  '--user show --property=ConsistsOf --value carillon.target') echo "$services" ;;
  '--user start carillon.target') running || start ;;
  # With its services, as Carillon stops it, or alone, as a test's cleanup does.
  '--user stop carillon.target'*) stop ;;
  '--user daemon-reload') [ "${FAKE_RELOAD:-ok}" != fail ] || exit 1 ;;
  '--user disable --now carillon.target') stop ;;
  '--user enable carillon.target') ;;
  *) exit 2 ;;
esac
