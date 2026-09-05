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
CARILLON_HOME=$(realpath -m -- "$CARILLON_HOME")
GLOBAL_PI=$(realpath -m -- "$HOME/.pi")
case "$CARILLON_HOME/" in "$GLOBAL_PI/"*) echo "CARILLON_HOME must not overlap the owner's global ~/.pi directory" >&2; exit 1 ;; esac
case "$GLOBAL_PI/" in "$CARILLON_HOME/"*) echo "CARILLON_HOME must not overlap the owner's global ~/.pi directory" >&2; exit 1 ;; esac

SOURCE=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
OWNER=$(id -un)
APP="$CARILLON_HOME/app"
APP_NEW=
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
  # shellcheck disable=SC2086
  "$SOURCE/bin/carillon" configure-discord --config "$CARILLON_HOME/carillon.toml" \
    --guild-id "$GUILD_ID" --bot-user-id "$BOT_USER_ID" $USER_IDS $ROUTES
fi

install -d -m 0700 "$CARILLON_HOME" "$CARILLON_HOME/credentials" "$CARILLON_HOME/pi" \
  "$CARILLON_HOME/logs"
APP_NEW=$(mktemp -d "$CARILLON_HOME/.app-new.XXXXXX")
CLEANUP_APP_NEW=1
cleanup_staging() {
  if [ -n "$APP_NEW" ] && [ -d "$APP_NEW" ]; then
    if [ "$CLEANUP_APP_NEW" -eq 1 ]; then
      rm -rf -- "$APP_NEW"
    else
      echo "application publication unconfirmed; inspect $APP and retained tree $APP_NEW; rerun installer before restarting" >&2
    fi
  fi
}
trap cleanup_staging EXIT
trap 'exit 1' HUP INT TERM
install -d -m 0700 "$APP_NEW/bin" "$APP_NEW/licenses" "$APP_NEW/systemd"
cp -R "$SOURCE/bin/." "$APP_NEW/bin/"
cp -R "$SOURCE/licenses/." "$APP_NEW/licenses/"
cp -R "$SOURCE/systemd/." "$APP_NEW/systemd/"
install -m 0755 "$SOURCE/uninstall.sh" "$APP_NEW/bin/carillon-uninstall"
printf '%s\n' "$APP_MARKER_VALUE" >"$APP_NEW/$APP_MARKER"
chmod -R go-rwx "$APP_NEW"
find "$APP_NEW/bin" -maxdepth 1 -type f -perm -0100 -exec chmod 0755 {} \;
# GNU sync -f uses Linux syncfs: persist all copied content, modes and directory
# entries on the staging filesystem before selecting it. No recursive fsync helper.
sync -f -- "$APP_NEW"
if [ -e "$APP" ] || [ -L "$APP" ]; then
  is_managed_app "$APP" || {
    echo "refusing to replace application path after ownership changed: $APP" >&2
    exit 1
  }
  # Keep both trees until publication is durable, even if mv succeeds but then
  # reports failure or a signal interrupts the shell before the following sync.
  CLEANUP_APP_NEW=0
  mv --exchange --no-copy -T -- "$APP_NEW" "$APP"
else
  CLEANUP_APP_NEW=0
  mv --no-copy -T -- "$APP_NEW" "$APP"
  APP_NEW=
fi
sync -f -- "$CARILLON_HOME"
CLEANUP_APP_NEW=1
[ -e "$CARILLON_HOME/credentials/discord-bot.token" ] || install -m 0600 /dev/null "$CARILLON_HOME/credentials/discord-bot.token"
chmod 0700 "$CARILLON_HOME" "$CARILLON_HOME/credentials" "$CARILLON_HOME/pi" "$CARILLON_HOME/logs"
chmod 0600 "$CARILLON_HOME/carillon.toml" "$CARILLON_HOME/credentials"/*.token

if "$APP/bin/carillon-register-services"; then
  :
else
  registration_result=$?
  if [ "$registration_result" -eq 10 ] && [ -n "$APP_NEW" ]; then
    # The helper confirmed the old definitions are intact. Select their old
    # application too; never infer rollback safety from an arbitrary failure.
    CLEANUP_APP_NEW=0
    if mv --exchange --no-copy -T -- "$APP_NEW" "$APP" && sync -f -- "$CARILLON_HOME"; then
      CLEANUP_APP_NEW=1
      echo "service registration failed; previous application and units restored" >&2
    else
      echo "application rollback durability unconfirmed; inspect $APP and retained tree $APP_NEW before restarting" >&2
    fi
  elif [ -n "$APP_NEW" ]; then
    echo "service registration did not confirm rollback; new application remains selected, previous tree retained at $APP_NEW; rerun installer before restarting" >&2
    APP_NEW=
  fi
  exit 1
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
