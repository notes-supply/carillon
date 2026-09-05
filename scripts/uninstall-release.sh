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
APP="$CARILLON_HOME/app"
APP_MARKER=.carillon-managed
APP_MARKER_VALUE=carillon-managed-application-v1

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
    sync -f -- "$CARILLON_HOME"
    if [ "${CARILLON_SKIP_SYSTEMD_RELOAD:-0}" != "1" ]; then
      systemctl --user daemon-reload
    fi
    printf '%s\n' "Application files already removed. Data remains at $CARILLON_HOME."
    exit 0
  fi
  echo "refusing to remove unowned application path: $APP" >&2
  exit 1
}

if [ "${CARILLON_SKIP_SYSTEMD_RELOAD:-0}" != "1" ]; then
  systemctl --user disable --now carillon.target
fi
for unit in \
  carillon.target \
  carillon-core.service \
  carillon-runtime-pi.service \
  carillon-discord.service
do
  rm -f "$UNIT_DIR/$unit"
done
[ ! -d "$UNIT_DIR" ] || sync -f -- "$UNIT_DIR"
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
if [ "${CARILLON_SKIP_SYSTEMD_RELOAD:-0}" != "1" ]; then
  systemctl --user daemon-reload
fi

printf '%s\n' "Carillon user services and application files removed. Data remains at $CARILLON_HOME."
