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
# An unrelated user unit shares the directory and must survive every uninstall.
printf '[Unit]\nDescription=unrelated\n' >"$UNIT_DIR/unrelated.service"
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
  systemctl)
    printf '%s\n' "$*" >>"$MARKER.systemctl"
    [ "$FAILURE" != relative ] || { touch "$MARKER"; exit 1; }
    [ "$FAILURE" != manager-failure ] || exit 1
    case "$*" in
      '--user disable --now carillon.target')
        [ "$FAILURE" != disable-failure ] || exit 1
        [ -f "$XDG_CONFIG_HOME/systemd/user/carillon.target" ] || {
          echo 'Unit carillon.target does not exist' >&2
          exit 1
        }
        ;;
      '--user show --property=LoadState --value carillon.target')
        if [ -f "$XDG_CONFIG_HOME/systemd/user/carillon.target" ] || [ -f "$ROOT/manager-loaded" ]; then
          printf '%s\n' loaded
        else
          printf '%s\n' not-found
        fi
        ;;
      '--user show --property=ActiveState --value carillon.target')
        if [ "$FAILURE" = active-missing ]; then printf '%s\n' active; else printf '%s\n' inactive; fi
        ;;
      '--user daemon-reload')
        [ "$FAILURE" != reload-failure ] || exit 1
        rm -f "$ROOT/manager-loaded"
        ;;
      *) exit 2 ;;
    esac
    exit 0
    ;;
esac
exit 2
COMMAND
chmod 0755 "$COMMANDS/fail-command"
for command in find rm rmdir sync systemctl; do
  ln -s fail-command "$COMMANDS/$command"
done
ln -s "$ROOT/app/bin/carillon-uninstall" "$OWNER_HOME/remove-carillon"
for failure in unset stale partial-term partial-kill sync-cleanup marker-kill rmdir-kill; do
  install_release "$ROOT"
  # The operator explicitly enabled the optional T3 unit: its link lives in the
  # target's wants directory, which disabling the target does not touch.
  mkdir -p "$UNIT_DIR/carillon.target.wants"
  ln -s ../carillon-runtime-t3.service "$UNIT_DIR/carillon.target.wants/carillon-runtime-t3.service"
  # The manager retains its loaded definition until a successful reload, even
  # when interrupted filesystem cleanup has already removed every unit file.
  touch "$ROOT/manager-loaded"
  printf '%s\n' private-token >"$ROOT/credentials/discord-bot.token"
  cp -a "$ROOT/credentials" "$TEST_ROOT/credentials-before"
  cp "$ROOT/carillon.toml" "$TEST_ROOT/config-before"
  file_count=$(find "$ROOT/app" -type f | wc -l)
  result=0
  # Both an absent variable and an inherited wrong root must select the installed
  # helper's canonical location, not the other managed default installation.
  if [ "$failure" = unset ]; then
    env -u CARILLON_HOME HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
      CARILLON_SKIP_SYSTEMD_RELOAD=0 PATH="$COMMANDS:$PATH" \
      ROOT="$ROOT" FAILURE="$failure" MARKER="$TEST_ROOT/$failure" \
      "$OWNER_HOME/remove-carillon" >"$TEST_ROOT/uninstall.log" 2>&1 || result=$?
  else
    env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
      CARILLON_HOME="$DECOY" CARILLON_SKIP_SYSTEMD_RELOAD=0 PATH="$COMMANDS:$PATH" \
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
    CARILLON_HOME="$ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=0 PATH="$COMMANDS:$PATH" \
    ROOT="$ROOT" FAILURE=recovery MARKER="$TEST_ROOT/recovery-$failure" \
    "$RELEASE/uninstall.sh" >/dev/null
  case "$failure" in
    partial-* | sync-cleanup)
      printf '%s\n' '--user disable --now carillon.target' \
        '--user daemon-reload' \
        '--user show --property=LoadState --value carillon.target' \
        '--user show --property=ActiveState --value carillon.target' \
        '--user daemon-reload' >"$TEST_ROOT/expected-recovery"
      cmp "$TEST_ROOT/expected-recovery" "$TEST_ROOT/recovery-$failure.systemctl"
      ;;
  esac
  [ ! -e "$ROOT/app" ]
  [ -z "$(find "$UNIT_DIR" -maxdepth 1 -name 'carillon*')" ]
  [ ! -e "$UNIT_DIR/carillon.target.wants/carillon-runtime-t3.service" ]
  [ -f "$UNIT_DIR/unrelated.service" ]
  cmp "$TEST_ROOT/config-before" "$ROOT/carillon.toml"
  diff -r "$TEST_ROOT/credentials-before" "$ROOT/credentials"
  rm -rf "$TEST_ROOT/credentials-before"
  diff -r --no-dereference "$TEST_ROOT/decoy-before" "$DECOY"
  [ "$(readlink "$OWNER_HOME/.local/bin/carillon")" = "$DECOY/app/bin/carillon" ]
  [ "$(cat "$OWNER_HOME/unrelated")" = unrelated-owner-data ]
  printf 'uninstall recovery passed: %s\n' "$failure"
done

# A real manager/disable failure or a still-active missing target must not be
# mistaken for an already-stopped installation. Even an inherited installer-only
# skip flag must not bypass shutdown. Keep every file for safe retry.
for failure in manager-failure disable-failure active-missing reload-failure; do
  install_release "$ROOT"
  case "$failure" in
    active-missing | reload-failure) rm "$UNIT_DIR/carillon.target" ;;
  esac
  cp -a "$ROOT" "$TEST_ROOT/refusal-before"
  cp -a "$UNIT_DIR" "$TEST_ROOT/refusal-units"
  app_inode=$(stat -c %i "$ROOT/app")
  if env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
    CARILLON_HOME="$ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=1 PATH="$COMMANDS:$PATH" \
    ROOT="$ROOT" FAILURE="$failure" MARKER="$TEST_ROOT/$failure" \
    "$RELEASE/uninstall.sh" >"$TEST_ROOT/refusal.log" 2>&1; then
    echo "uninstall ignored service-manager failure: $failure" >&2
    exit 1
  fi
  [ "$(stat -c %i "$ROOT/app")" = "$app_inode" ]
  diff -r --no-dereference "$TEST_ROOT/refusal-before" "$ROOT"
  diff -r --no-dereference "$TEST_ROOT/refusal-units" "$UNIT_DIR"
  env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
    CARILLON_HOME="$ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=0 PATH="$COMMANDS:$PATH" \
    ROOT="$ROOT" FAILURE=recovery MARKER="$TEST_ROOT/repaired-$failure" \
    "$RELEASE/uninstall.sh" >/dev/null
  [ ! -e "$ROOT/app" ]
  rm -rf "$TEST_ROOT/refusal-before" "$TEST_ROOT/refusal-units"
  printf 'uninstall manager refusal and retry passed: %s\n' "$failure"
done

# Refuse relative owner/unit paths before service-manager or filesystem changes.
install_release "$ROOT"
cp -a "$ROOT" "$TEST_ROOT/installation-before"
cp -a "$UNIT_DIR" "$TEST_ROOT/units-before-relative"
app_inode=$(stat -c %i "$ROOT/app")
mkdir -p "$TEST_ROOT/relative-config/systemd" "$TEST_ROOT/relative-home/.config/systemd"
cp -a "$UNIT_DIR" "$TEST_ROOT/relative-config/systemd/user"
cp -a "$UNIT_DIR" "$TEST_ROOT/relative-home/.config/systemd/user"
for helper in "$OWNER_HOME/remove-carillon" "$RELEASE/uninstall.sh"; do
  for invalid in xdg home home-with-xdg; do
    bad_home=$OWNER_HOME
    bad_xdg=relative-config
    expected=XDG_CONFIG_HOME
    if [ "$invalid" != xdg ]; then
      bad_home=relative-home
      bad_xdg=
      expected=HOME
      [ "$invalid" != home-with-xdg ] || bad_xdg=$OWNER_HOME/.config
    fi
    if (cd "$TEST_ROOT" && env HOME="$bad_home" XDG_CONFIG_HOME="$bad_xdg" \
      CARILLON_HOME="$ROOT" CARILLON_SKIP_SYSTEMD_RELOAD=0 PATH="$COMMANDS:$PATH" \
      FAILURE=relative MARKER="$TEST_ROOT/service-manager-called" \
      "$helper") >"$TEST_ROOT/relative.log" 2>&1; then
      echo "uninstaller accepted relative paths: $helper / $invalid" >&2
      exit 1
    fi
    grep -F "$expected must be an absolute path" "$TEST_ROOT/relative.log" >/dev/null
    [ ! -e "$TEST_ROOT/service-manager-called" ]
    [ "$(stat -c %i "$ROOT/app")" = "$app_inode" ]
    diff -r --no-dereference "$TEST_ROOT/installation-before" "$ROOT"
    for units in "$UNIT_DIR" "$TEST_ROOT/relative-config/systemd/user" "$TEST_ROOT/relative-home/.config/systemd/user"; do
      diff -r --no-dereference "$TEST_ROOT/units-before-relative" "$units"
    done
    printf 'uninstall path refusal passed: %s / %s\n' "${helper##*/}" "$invalid"
  done
done
env HOME="$OWNER_HOME" XDG_CONFIG_HOME="$OWNER_HOME/.config" \
  PATH="$COMMANDS:$PATH" ROOT="$ROOT" FAILURE=recovery MARKER="$TEST_ROOT/path-recovery" \
  "$OWNER_HOME/remove-carillon" >/dev/null
[ ! -e "$ROOT/app" ]
[ -z "$(find "$UNIT_DIR" -maxdepth 1 -name 'carillon*')" ]
diff -r --no-dereference "$TEST_ROOT/decoy-before" "$DECOY"

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
