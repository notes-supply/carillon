#!/bin/sh
set -eu
umask 077

usage() {
  echo "usage: install.sh --guild-id ID --bot-user-id ID --user-id ID [--user-id ID ...] --route CHANNEL_ID:mention|always:thread|channel [--route ...] [--base-port PORT] [--workspace PATH]" >&2
  exit 2
}

GUILD_ID=
BOT_USER_ID=
USER_IDS=
ROUTES=
BASE_PORT=43800
WORKSPACE=${HOME:-}
while [ "$#" -gt 0 ]; do
  case "$1" in
    --guild-id) [ "$#" -ge 2 ] || usage; GUILD_ID=$2; shift 2 ;;
    --bot-user-id) [ "$#" -ge 2 ] || usage; BOT_USER_ID=$2; shift 2 ;;
    --user-id) [ "$#" -ge 2 ] || usage; USER_IDS="$USER_IDS --user-id $2"; shift 2 ;;
    --route) [ "$#" -ge 2 ] || usage; ROUTES="$ROUTES --route $2"; shift 2 ;;
    --base-port) [ "$#" -ge 2 ] || usage; BASE_PORT=$2; shift 2 ;;
    --workspace) [ "$#" -ge 2 ] || usage; WORKSPACE=$2; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$GUILD_ID" ] || usage
[ -n "$BOT_USER_ID" ] || usage
[ -n "$USER_IDS" ] || usage
[ -n "$ROUTES" ] || usage
[ "$(id -u)" -ne 0 ] || { echo "installer must run as the intended non-root owner; root ownership is ambiguous" >&2; exit 1; }
[ -n "${HOME:-}" ] && [ -d "$HOME" ] || { echo "HOME must name the installing user's existing home directory" >&2; exit 1; }
[ "$(stat -c %u "$HOME")" -eq "$(id -u)" ] || { echo "HOME is not owned by the installing user" >&2; exit 1; }

case "$HOME:$WORKSPACE:${CARILLON_HOME:-}" in
  *'
'*) echo "HOME, workspace, and CARILLON_HOME must not contain newlines" >&2; exit 1 ;;
esac
case "$WORKSPACE" in /*) ;; *) echo "workspace must be an absolute path" >&2; exit 1 ;; esac
[ -d "$WORKSPACE" ] || { echo "workspace does not exist: $WORKSPACE" >&2; exit 1; }

CARILLON_HOME=${CARILLON_HOME:-"$HOME/.carillon"}
case "$CARILLON_HOME" in /*) ;; *) echo "CARILLON_HOME must be an absolute path" >&2; exit 1 ;; esac
case "$CARILLON_HOME/" in "$HOME/.pi/"*) echo "CARILLON_HOME must not overlap the owner's global ~/.pi directory" >&2; exit 1 ;; esac

SOURCE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
OWNER=$(id -un)
OWNER_SHELL=$(getent passwd "$OWNER" | cut -d: -f7)
[ -n "$OWNER_SHELL" ] && [ -x "$OWNER_SHELL" ] || { echo "cannot resolve the installing user's login shell" >&2; exit 1; }
OWNER_PATH=${PATH:-/usr/local/bin:/usr/bin:/bin}
UNIT_DIR=${XDG_CONFIG_HOME:-"$HOME/.config"}/systemd/user
APP="$CARILLON_HOME/app"
APP_NEW=
APP_OLD=
CONFIG_OLD=
APP_MARKER=.carillon-managed
APP_MARKER_VALUE=carillon-managed-application-v1

cleanup_staging() {
  [ -z "$APP_NEW" ] || rm -rf -- "$APP_NEW"
  if [ -n "$APP_OLD" ]; then
    if [ ! -e "$APP" ] && [ ! -L "$APP" ]; then
      mv "$APP_OLD" "$APP"
    else
      rm -rf -- "$APP_OLD"
    fi
  fi
  if [ -n "$CONFIG_OLD" ]; then
    mv "$CONFIG_OLD" "$CARILLON_HOME/carillon.toml"
  fi
}
trap cleanup_staging EXIT

is_managed_app() {
  candidate=$1
  [ -d "$candidate" ] &&
    [ ! -L "$candidate" ] &&
    [ -f "$candidate/$APP_MARKER" ] &&
    [ ! -L "$candidate/$APP_MARKER" ] &&
    [ "$(cat "$candidate/$APP_MARKER")" = "$APP_MARKER_VALUE" ]
}

if [ -e "$APP" ] || [ -L "$APP" ]; then
  is_managed_app "$APP" || {
    echo "refusing to replace unowned application path: $APP" >&2
    exit 1
  }
fi

if [ ! -f "$CARILLON_HOME/carillon.toml" ]; then
  # USER_IDS and ROUTES intentionally expand into repeated, validated CLI arguments.
  # shellcheck disable=SC2086
  "$SOURCE/bin/carillon" init --root "$CARILLON_HOME" --workspace "$WORKSPACE" \
    --guild-id "$GUILD_ID" --bot-user-id "$BOT_USER_ID" $USER_IDS $ROUTES --base-port "$BASE_PORT"
else
  # Existing per-user installations retain credentials and durable state.
  CONFIG_OLD=$(mktemp "$CARILLON_HOME/.carillon-old.XXXXXX")
  cp -p "$CARILLON_HOME/carillon.toml" "$CONFIG_OLD" || { rm -f -- "$CONFIG_OLD"; CONFIG_OLD=; exit 1; }
  # shellcheck disable=SC2086
  "$SOURCE/bin/carillon" configure-discord --config "$CARILLON_HOME/carillon.toml" \
    --guild-id "$GUILD_ID" --bot-user-id "$BOT_USER_ID" $USER_IDS $ROUTES
fi

install -d -m 0700 "$CARILLON_HOME" "$CARILLON_HOME/credentials" "$CARILLON_HOME/pi" \
  "$CARILLON_HOME/logs" "$UNIT_DIR"
APP_NEW=$(mktemp -d "$CARILLON_HOME/.app-new.XXXXXX")
install -d -m 0700 "$APP_NEW/bin" "$APP_NEW/licenses" "$APP_NEW/systemd"
cp -R "$SOURCE/bin/." "$APP_NEW/bin/"
cp -R "$SOURCE/licenses/." "$APP_NEW/licenses/"
cp -R "$SOURCE/systemd/." "$APP_NEW/systemd/"
install -m 0755 "$SOURCE/uninstall.sh" "$APP_NEW/bin/carillon-uninstall"
printf '%s\n' "$APP_MARKER_VALUE" >"$APP_NEW/$APP_MARKER"
chmod -R go-rwx "$APP_NEW"
find "$APP_NEW/bin" -maxdepth 1 -type f -perm -0100 -exec chmod 0755 {} \;
if [ -e "$APP" ] || [ -L "$APP" ]; then
  is_managed_app "$APP" || {
    echo "refusing to replace application path after ownership changed: $APP" >&2
    exit 1
  }
  APP_OLD=$(mktemp -d "$CARILLON_HOME/.app-old.XXXXXX")
  rmdir "$APP_OLD"
  mv "$APP" "$APP_OLD"
fi
mv "$APP_NEW" "$APP"
APP_NEW=
[ -z "$APP_OLD" ] || rm -rf -- "$APP_OLD"
APP_OLD=
[ -e "$CARILLON_HOME/credentials/discord-bot.token" ] || install -m 0600 /dev/null "$CARILLON_HOME/credentials/discord-bot.token"
chmod 0700 "$CARILLON_HOME" "$CARILLON_HOME/credentials" "$CARILLON_HOME/pi" "$CARILLON_HOME/logs"
chmod 0600 "$CARILLON_HOME/carillon.toml" "$CARILLON_HOME/credentials"/*.token

unit_escape() {
  printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g' -e 's/%/%%/g'
}
sed_replacement() {
  printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'
}
HOME_UNIT=$(sed_replacement "$(unit_escape "$HOME")")
OWNER_UNIT=$(sed_replacement "$(unit_escape "$OWNER")")
ROOT_UNIT=$(sed_replacement "$(unit_escape "$CARILLON_HOME")")
SHELL_UNIT=$(sed_replacement "$(unit_escape "$OWNER_SHELL")")
PATH_UNIT=$(sed_replacement "$(unit_escape "$OWNER_PATH")")
for template in "$SOURCE"/systemd/*; do
  unit=$UNIT_DIR/$(basename "$template")
  sed \
    -e "s|@OWNER_HOME@|$HOME_UNIT|g" \
    -e "s|@OWNER@|$OWNER_UNIT|g" \
    -e "s|@CARILLON_HOME@|$ROOT_UNIT|g" \
    -e "s|@OWNER_SHELL@|$SHELL_UNIT|g" \
    -e "s|@OWNER_PATH@|$PATH_UNIT|g" \
    "$template" >"$unit"
  chmod 0600 "$unit"
done

if [ "${CARILLON_SKIP_SYSTEMD_RELOAD:-0}" != "1" ]; then
  systemctl --user daemon-reload
  systemctl --user enable carillon.target
fi
if [ -n "$CONFIG_OLD" ]; then
  rm -f -- "$CONFIG_OLD"
  CONFIG_OLD=
fi

cat <<EOF
Carillon installed for $OWNER at $CARILLON_HOME and enabled for this user's service manager.
1. Put the Discord bot token in $CARILLON_HOME/credentials/discord-bot.token (mode 0600).
2. Authenticate packaged Pi state with:
   $APP/bin/carillon-pi --no-session --no-tools --no-approve
3. Start deliberately with: systemctl --user start carillon.target
4. Inspect services with: systemctl --user status carillon.target
   Inspect the running product with: $APP/bin/carillon status --config $CARILLON_HOME/carillon.toml
For boot without an interactive login, an administrator may run: loginctl enable-linger $OWNER
EOF
