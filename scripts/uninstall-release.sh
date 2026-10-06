#!/bin/sh
set -eu

[ "$(id -u)" -ne 0 ] || { echo "uninstaller must run as the non-root owner" >&2; exit 1; }
: "${HOME:?HOME is required}"
# The installed helper selects its own installation, including through a symlink.
# The release archive's standalone uninstall.sh retains explicit root selection
# for recovery when interrupted removal has already deleted the installed helper.
SELF=$(realpath -e -- "$0")
case "$SELF" in
  */app/bin/carillon-uninstall) CARILLON_HOME=$(dirname -- "$(dirname -- "$(dirname -- "$SELF")")") ;;
  *) CARILLON_HOME=${CARILLON_HOME:-"$HOME/.carillon"} ;;
esac
case "$CARILLON_HOME" in /*) ;; *) echo "CARILLON_HOME must be an absolute path" >&2; exit 1 ;; esac
CARILLON_HOME=$(realpath -m -- "$CARILLON_HOME")
UNIT_DIR=${XDG_CONFIG_HOME:-"$HOME/.config"}/systemd/user
case "$HOME" in /*) ;; *) echo "HOME must be an absolute path" >&2; exit 1 ;; esac
case "$UNIT_DIR" in /*) ;; *) echo "XDG_CONFIG_HOME must be an absolute path" >&2; exit 1 ;; esac
APP="$CARILLON_HOME/app"
APP_MARKER=.carillon-managed
APP_MARKER_VALUE=carillon-managed-application-v1
ENTRY=$HOME/.local/bin/carillon

# Serialize with updates and reinstallation of the same installation.
if [ -d "$CARILLON_HOME" ]; then
  exec 9>>"$CARILLON_HOME/.maintenance.lock"
  flock -n 9 || { echo "another Carillon maintenance operation is running for $CARILLON_HOME" >&2; exit 1; }
fi

# Every installation of this owner shares the user units. Hold their lock,
# after this installation's, through every service-manager call below,
# including the reload that finishes an already removed application.
(umask 077 && mkdir -p -- "$UNIT_DIR" && : >>"$UNIT_DIR/.carillon-units.lock")
UNIT_DIR=$(realpath -e -- "$UNIT_DIR")
exec 8>>"$UNIT_DIR/.carillon-units.lock"
flock -n 8 || { echo "another Carillon operation is changing the user units in $UNIT_DIR" >&2; exit 1; }

# Remove the owner's PATH entry point only when it runs this installation.
remove_entry_point() {
  if [ -L "$ENTRY" ] && [ "$(readlink -- "$ENTRY")" = "$APP/bin/carillon" ]; then
    rm -f -- "$ENTRY"
  fi
}

is_managed_app() {
  candidate=$1
  [ -d "$candidate" ] &&
    [ ! -L "$candidate" ] &&
    [ -f "$candidate/$APP_MARKER" ] &&
    [ ! -L "$candidate/$APP_MARKER" ] &&
    [ "$(cat "$candidate/$APP_MARKER")" = "$APP_MARKER_VALUE" ]
}

is_managed_app "$APP" || {
  # A crash after the final marker unlink leaves only an empty directory (or no
  # directory). Without the marker, never recurse or remove service definitions.
  if [ ! -L "$APP" ] && { [ ! -e "$APP" ] || rmdir -- "$APP" 2>/dev/null; }; then
    remove_entry_point
    sync -f -- "$CARILLON_HOME"
    systemctl --user daemon-reload
    printf '%s\n' "Application files already removed. Data remains at $CARILLON_HOME."
    exit 0
  fi
  echo "refusing to remove unowned application path: $APP" >&2
  exit 1
}

# The user units may run another installation of the same owner; leave them
# (and its services) alone. The registration script writes this exact line.
unit_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/%/%%/g'
}
OWN_UNITS=1
if [ -f "$UNIT_DIR/carillon-core.service" ] &&
  ! grep -Fqx "Environment=\"CARILLON_HOME=$(unit_escape "$CARILLON_HOME")\"" "$UNIT_DIR/carillon-core.service"
then
  OWN_UNITS=0
  echo "Carillon's user units run another installation; leaving them registered."
fi

if [ "$OWN_UNITS" -eq 1 ] && ! systemctl --user disable --now carillon.target; then
  # An interrupted uninstall can already have removed the definitions. Only
  # confirmed absence/inactivity permits cleanup; manager/stop failures do not.
  # Definitions may be gone while the manager still caches the old unit.
  systemctl --user daemon-reload || exit 1
  load_state=$(systemctl --user show --property=LoadState --value carillon.target) &&
    [ "$load_state" = not-found ] &&
    active_state=$(systemctl --user show --property=ActiveState --value carillon.target) &&
    [ "$active_state" = inactive ] || exit 1
fi
if [ "$OWN_UNITS" -eq 1 ]; then
  for unit in \
    carillon.target \
    carillon-core.service \
    carillon-runtime-pi.service \
    carillon-discord.service
  do
    rm -f "$UNIT_DIR/$unit"
  done
  [ ! -d "$UNIT_DIR" ] || sync -f -- "$UNIT_DIR"
fi
remove_entry_point
is_managed_app "$APP" || {
  echo "refusing to remove application path after ownership changed: $APP" >&2
  exit 1
}
# Retain ownership evidence throughout recursive cleanup. The release's
# standalone helper can retry even if this installed script has been deleted.
(CDPATH='' cd -- "$APP" && find . -depth -mindepth 1 ! -path "./$APP_MARKER" -delete)
sync -f -- "$APP"
rm -- "$APP/$APP_MARKER"
rmdir -- "$APP"
sync -f -- "$CARILLON_HOME"
systemctl --user daemon-reload

printf '%s\n' "Carillon user services and application files removed. Data remains at $CARILLON_HOME."
