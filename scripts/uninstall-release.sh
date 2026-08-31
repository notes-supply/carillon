#!/bin/sh
set -eu

[ "$(id -u)" -ne 0 ] || { echo "uninstaller must run as the non-root owner" >&2; exit 1; }
: "${HOME:?HOME is required}"
CARILLON_HOME=${CARILLON_HOME:-"$HOME/.carillon"}
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
is_managed_app "$APP" || {
  echo "refusing to remove application path after ownership changed: $APP" >&2
  exit 1
}
rm -rf -- "$APP"
if [ "${CARILLON_SKIP_SYSTEMD_RELOAD:-0}" != "1" ]; then
  systemctl --user daemon-reload
fi

printf '%s\n' "Carillon user services and application files removed. Data remains at $CARILLON_HOME."
