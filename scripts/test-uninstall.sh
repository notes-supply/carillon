#!/bin/sh
set -eu

RELEASE=$(realpath -e -- "${1:?release directory is required}")
TEST_ROOT=$(mktemp -d)
trap 'rm -rf -- "$TEST_ROOT"' EXIT
trap 'exit 1' HUP INT TERM
OWNER_HOME=$TEST_ROOT/owner
ROOT=$OWNER_HOME/'relocated [agent]'
DECOY=$OWNER_HOME/.carillon
UNIT_DIR=$OWNER_HOME/.config/systemd/user
COMMANDS=$TEST_ROOT/commands
mkdir -p "$OWNER_HOME/workspace" "$COMMANDS"

install_release() {
  env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
    CARILLON_HOME="$1" CARILLON_SKIP_SYSTEMD_RELOAD=1 \
    "$RELEASE/install.sh" --guild-id 100 --bot-user-id 201 --user-id 200 \
    --route 300:always:channel --workspace "$OWNER_HOME/workspace" >/dev/null
}
install_release "$DECOY"
printf '%s\n' unrelated-owner-data >"$OWNER_HOME/unrelated"
cp -a "$DECOY" "$TEST_ROOT/decoy-before"
REAL_FIND=$(command -v find)
REAL_RM=$(command -v rm)
REAL_RMDIR=$(command -v rmdir)
REAL_SYNC=$(command -v sync)
export REAL_FIND REAL_RM REAL_RMDIR REAL_SYNC
cat >"$COMMANDS/fail-command" <<'COMMAND'
#!/bin/sh
set -eu
command=${0##*/}
last=
for argument do last=$argument; done
case "$command:$FAILURE:$last" in
  find:partial-term:-delete | find:partial-kill:-delete)
    signal=SIGTERM
    [ "$FAILURE" != partial-kill ] || signal=SIGKILL
    printf '%s\n' "$FAILURE" >"$MARKER"
    exec strace -qq -yy -o "$MARKER.trace" -e trace=unlinkat \
      -e inject="unlinkat:signal=$signal:when=3" "$REAL_FIND" "$@"
    ;;
  sync:sync-cleanup:"$ROOT/app")
    [ "$(find "$ROOT/app" -mindepth 1 | wc -l)" -eq 1 ]
    [ -f "$ROOT/app/.carillon-managed" ]
    printf '%s\n' "$FAILURE" >"$MARKER"
    exec strace -qq -yy -o "$MARKER.trace" -e trace=syncfs \
      -e inject=syncfs:error=EIO:when=1 "$REAL_SYNC" "$@"
    ;;
  rm:marker-kill:"$ROOT/app/.carillon-managed")
    "$REAL_RM" "$@"
    printf '%s\n' "$FAILURE" >"$MARKER"
    kill -KILL "$PPID"
    exit 1
    ;;
  rmdir:rmdir-kill:"$ROOT/app")
    "$REAL_RMDIR" "$@"
    printf '%s\n' "$FAILURE" >"$MARKER"
    kill -KILL "$PPID"
    exit 1
    ;;
esac
case "$command" in
  find) exec "$REAL_FIND" "$@" ;;
  rm) exec "$REAL_RM" "$@" ;;
  rmdir) exec "$REAL_RMDIR" "$@" ;;
  sync) exec "$REAL_SYNC" "$@" ;;
esac
exit 2
COMMAND
chmod 0755 "$COMMANDS/fail-command"
for command in find rm rmdir sync; do
  ln -s fail-command "$COMMANDS/$command"
done
ln -s "$ROOT/app/bin/carillon-uninstall" "$OWNER_HOME/remove-carillon"
for failure in unset stale partial-term partial-kill sync-cleanup marker-kill rmdir-kill; do
  install_release "$ROOT"
  printf '%s\n' private-token >"$ROOT/credentials/discord-bot.token"
  cp -a "$ROOT/credentials" "$TEST_ROOT/credentials-before"
  cp "$ROOT/carillon.toml" "$TEST_ROOT/config-before"
  file_count=$(find "$ROOT/app" -type f | wc -l)
  result=0
  # Both an absent variable and an inherited wrong root must select the installed
  # helper's canonical location, not the other managed default installation.
  if [ "$failure" = unset ]; then
    env -u CARILLON_HOME HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
      CARILLON_SKIP_SYSTEMD_RELOAD=1 "$OWNER_HOME/remove-carillon" >"$TEST_ROOT/uninstall.log" 2>&1 || result=$?
  else
    env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
      CARILLON_HOME="$DECOY" CARILLON_SKIP_SYSTEMD_RELOAD=1 PATH="$COMMANDS:$PATH" \
      ROOT="$ROOT" FAILURE="$failure" MARKER="$TEST_ROOT/$failure" \
      "$OWNER_HOME/remove-carillon" >"$TEST_ROOT/uninstall.log" 2>&1 || result=$?
  fi
  case "$failure" in
    unset | stale) [ "$result" -eq 0 ] ;;
    *)
      [ "$result" -ne 0 ]
      [ -f "$TEST_ROOT/$failure" ] || { cat "$TEST_ROOT/uninstall.log" >&2; exit 1; }
      [ "$(cat "$TEST_ROOT/$failure")" = "$failure" ]
      case "$failure" in
        partial-*)
          [ "$(cat "$ROOT/app/.carillon-managed")" = carillon-managed-application-v1 ]
          remaining=$(find "$ROOT/app" -type f | wc -l)
          [ "$remaining" -gt 1 ] && [ "$remaining" -lt "$file_count" ]
          grep -F 'killed by SIG' "$TEST_ROOT/$failure.trace" >/dev/null
          ;;
        sync-cleanup)
          [ -f "$ROOT/app/.carillon-managed" ]
          grep -F 'EIO (Input/output error) (INJECTED)' "$TEST_ROOT/$failure.trace" >/dev/null
          ;;
        marker-kill) [ "$(find "$ROOT/app" -mindepth 1 | wc -l)" -eq 0 ] ;;
        rmdir-kill) [ ! -e "$ROOT/app" ] ;;
      esac
      ;;
  esac
  # The archive helper remains available even after the installed one is gone.
  env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
    CARILLON_HOME="$ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=1 "$RELEASE/uninstall.sh" >/dev/null
  [ ! -e "$ROOT/app" ]
  [ -z "$(find "$UNIT_DIR" -maxdepth 1 -name 'carillon*')" ]
  cmp "$TEST_ROOT/config-before" "$ROOT/carillon.toml"
  diff -r "$TEST_ROOT/credentials-before" "$ROOT/credentials"
  rm -rf "$TEST_ROOT/credentials-before"
  diff -r --no-dereference "$TEST_ROOT/decoy-before" "$DECOY"
  [ "$(cat "$OWNER_HOME/unrelated")" = unrelated-owner-data ]
  printf 'uninstall recovery passed: %s\n' "$failure"
done

# Missing ownership evidence must still refuse recursive deletion and preserve
# the current user units. Empty-path recovery above cannot be used for this tree.
install_release "$ROOT"
mv "$ROOT/app/.carillon-managed" "$ROOT/app/marker-saved"
cp -a "$UNIT_DIR" "$TEST_ROOT/units-before"
if env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
  CARILLON_HOME="$ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=1 "$RELEASE/uninstall.sh" >/dev/null 2>&1; then
  echo 'uninstaller accepted an unmarked nonempty application' >&2
  exit 1
fi
[ -x "$ROOT/app/bin/carillon" ]
diff -r "$TEST_ROOT/units-before" "$UNIT_DIR"
printf '%s\n' 'uninstall refusal passed: unmarked nonempty application'
