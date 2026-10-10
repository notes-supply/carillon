#!/bin/sh
set -eu
umask 077

usage() {
  echo "usage: install.sh [--runtime pi|t3] [--base-port PORT] [--workspace PATH]    (asks the rest at the terminal)" >&2
  echo "       install.sh [--runtime pi|t3] --guild-id ID --bot-user-id ID --user-id ID [--user-id ID ...] --route CHANNEL_ID:mention|always:thread|channel [--route ...] [--base-port PORT] [--workspace PATH]" >&2
  echo "       install.sh --update [--check]    (run by 'carillon update'; --check changes nothing)" >&2
  exit 2
}

# Setup without the Discord options asks for everything at the terminal
# (`carillon setup`); with them it writes the registry and Discord settings
# from these arguments and asks nothing. An update only publishes the
# application and service definitions; it never rewrites configuration,
# credentials or state.
MODE=setup
CHECK=0
case "$#:$*" in
  '1:--update') MODE=update; shift ;;
  '2:--update --check') MODE=update; CHECK=1; shift 2 ;;
esac
GUILD_ID=
BOT_USER_ID=
USER_IDS=
ROUTES=
BASE_PORT=43800
WORKSPACE=
RUNTIME=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --guild-id) [ "$#" -ge 2 ] || usage; GUILD_ID=$2; shift 2 ;;
    --bot-user-id) [ "$#" -ge 2 ] || usage; BOT_USER_ID=$2; shift 2 ;;
    --user-id) [ "$#" -ge 2 ] || usage; USER_IDS="$USER_IDS --user-id $2"; shift 2 ;;
    --route) [ "$#" -ge 2 ] || usage; ROUTES="$ROUTES --route $2"; shift 2 ;;
    --base-port) [ "$#" -ge 2 ] || usage; BASE_PORT=$2; shift 2 ;;
    --workspace) [ "$#" -ge 2 ] || usage; WORKSPACE=$2; shift 2 ;;
    --runtime)
      [ "$#" -ge 2 ] || usage
      case "$2" in pi | t3) RUNTIME=$2 ;; *) usage ;; esac
      shift 2 ;;
    *) usage ;;
  esac
done
GUIDED=0
if [ "$MODE" = setup ]; then
  if [ -z "$GUILD_ID$BOT_USER_ID$USER_IDS$ROUTES" ]; then
    GUIDED=1
  else
    [ -n "$GUILD_ID" ] || usage
    [ -n "$BOT_USER_ID" ] || usage
    [ -n "$USER_IDS" ] || usage
    [ -n "$ROUTES" ] || usage
    WORKSPACE=${WORKSPACE:-${HOME:-}}
  fi
fi
[ "$(id -u)" -ne 0 ] || { echo "installer must run as the intended non-root owner; root ownership is ambiguous" >&2; exit 1; }
[ -n "${HOME:-}" ] && [ -d "$HOME" ] || { echo "HOME must name the installing user's existing home directory" >&2; exit 1; }
[ "$(stat -c %u "$HOME")" -eq "$(id -u)" ] || { echo "HOME is not owned by the installing user" >&2; exit 1; }

case "$HOME:$WORKSPACE:${CARILLON_HOME:-}" in
  *'
'*) echo "HOME, workspace, and CARILLON_HOME must not contain newlines" >&2; exit 1 ;;
esac
if [ -n "$WORKSPACE" ]; then
  case "$WORKSPACE" in /*) ;; *) echo "workspace must be an absolute path" >&2; exit 1 ;; esac
  [ -d "$WORKSPACE" ] || { echo "workspace does not exist: $WORKSPACE" >&2; exit 1; }
fi

CARILLON_HOME=${CARILLON_HOME:-"$HOME/.carillon"}
case "$CARILLON_HOME" in /*) ;; *) echo "CARILLON_HOME must be an absolute path" >&2; exit 1 ;; esac
CARILLON_HOME=$(realpath -m -- "$CARILLON_HOME")
GLOBAL_PI=$(realpath -m -- "$HOME/.pi")
case "$CARILLON_HOME/" in "$GLOBAL_PI/"*) echo "CARILLON_HOME must not overlap the owner's global ~/.pi directory" >&2; exit 1 ;; esac
case "$GLOBAL_PI/" in "$CARILLON_HOME/"*) echo "CARILLON_HOME must not overlap the owner's global ~/.pi directory" >&2; exit 1 ;; esac
if [ "$MODE" = update ] && [ ! -f "$CARILLON_HOME/carillon.toml" ]; then
  echo "no Carillon installation at $CARILLON_HOME; run install.sh with setup options" >&2
  exit 1
fi

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

# The packager lists every shipped file in MANIFEST, marked `x` when it must be
# executable. Refuse a payload that differs before changing anything, rather
# than discovering a missing program after the services were replaced.
verify_payload() {
  manifest=$SOURCE/MANIFEST
  if [ ! -f "$manifest" ] || [ -L "$manifest" ]; then
    echo "release is incomplete: $manifest is missing" >&2
    return 1
  fi
  if [ -n "$(find "$SOURCE" ! -type f ! -type d)" ]; then
    echo "release contains links or special files" >&2
    return 1
  fi
  listed=0
  while IFS= read -r line; do
    kind=${line%% *}
    path=${line#* }
    listed=$((listed + 1))
    file=$SOURCE/$path
    if [ ! -f "$file" ] || [ -L "$file" ]; then
      echo "release is incomplete: $path is missing" >&2
      return 1
    fi
    case "$kind" in
      x) [ -x "$file" ] || { echo "release file $path is not executable" >&2; return 1; } ;;
      r) [ -r "$file" ] || { echo "release file $path is not readable" >&2; return 1; } ;;
      *) echo "release MANIFEST is malformed: $line" >&2; return 1 ;;
    esac
  done <"$manifest"
  # Every listed file exists, so equal counts mean nothing unlisted was added.
  if [ "$(find "$SOURCE" -type f ! -path "$SOURCE/MANIFEST" | wc -l)" -ne "$listed" ]; then
    echo "release contains files its MANIFEST does not list" >&2
    return 1
  fi
}
verify_payload

if [ "$CHECK" -eq 1 ]; then
  # Application replacement needs an atomic directory exchange here.
  probe=$(mktemp -d "$CARILLON_HOME/.update-probe.XXXXXX")
  mkdir "$probe/a" "$probe/b"
  if ! mv --exchange --no-copy -T -- "$probe/a" "$probe/b" 2>/dev/null; then
    rm -rf -- "$probe"
    echo "cannot exchange directories atomically in $CARILLON_HOME: GNU mv --exchange (coreutils 9.5+) and filesystem support are required" >&2
    exit 1
  fi
  rm -rf -- "$probe"
  # This release's own programs must accept the installation as it is now,
  # read-only: the registry and core database, and the Discord settings.
  "$SOURCE/bin/carillon" --home "$CARILLON_HOME" check
  "$SOURCE/bin/carillon" --home "$CARILLON_HOME" discord check --provider discord-private
  exit 0
fi

# Guided setup creates the installation for the chosen runtime and has each
# provider's program set its part up, asking at the terminal; it takes the
# maintenance lock only for its short writes. A rerun keeps what is set up
# when asked to.
if [ "$GUIDED" -eq 1 ]; then
  set -- --base-port "$BASE_PORT"
  [ -z "$RUNTIME" ] || set -- "$@" --runtime "$RUNTIME"
  [ -z "$WORKSPACE" ] || set -- "$@" --workspace "$WORKSPACE"
  "$SOURCE/bin/carillon" --home "$CARILLON_HOME" setup "$@"
fi
# Unattended setup keeps an existing registry, credentials and durable state;
# only the Discord provider's own settings file is (re)written from the
# arguments.
if [ "$MODE" = setup ] && [ "$GUIDED" -eq 0 ] && [ ! -f "$CARILLON_HOME/carillon.toml" ]; then
  "$SOURCE/bin/carillon" --home "$CARILLON_HOME" init --workspace "$WORKSPACE" \
    --base-port "$BASE_PORT" --runtime "${RUNTIME:-pi}"
fi
# Serialize maintenance with `carillon update`, which holds this lock itself
# while it runs `install.sh --update`, and with uninstallation.
if [ "${CARILLON_MAINTENANCE_LOCK:-}" != "$CARILLON_HOME" ]; then
  exec 9>>"$CARILLON_HOME/.maintenance.lock"
  flock -n 9 || { echo "another Carillon maintenance operation is running for $CARILLON_HOME" >&2; exit 1; }
  # The programs run below change the installation under this lock.
  CARILLON_MAINTENANCE_LOCK=$CARILLON_HOME
  export CARILLON_MAINTENANCE_LOCK
fi
if [ "$MODE" = setup ] && [ "$GUIDED" -eq 0 ]; then
  # USER_IDS and ROUTES intentionally expand into repeated, validated CLI arguments.
  # shellcheck disable=SC2086
  "$SOURCE/bin/carillon" --home "$CARILLON_HOME" discord configure --provider discord-private \
    --guild-id "$GUILD_ID" --bot-user-id "$BOT_USER_ID" $USER_IDS $ROUTES >/dev/null
fi

install -d -m 0700 "$CARILLON_HOME" "$CARILLON_HOME/credentials" "$CARILLON_HOME/providers" \
  "$CARILLON_HOME/logs"
APP_NEW=$(mktemp -d "$CARILLON_HOME/.app-new.XXXXXX")
CLEANUP_APP_NEW=1
cleanup_staging() {
  if [ -n "$APP_NEW" ] && [ -d "$APP_NEW" ]; then
    if [ "$CLEANUP_APP_NEW" -eq 1 ]; then
      rm -rf -- "$APP_NEW"
    else
      retry=installer
      [ "$MODE" = setup ] || retry='carillon update'
      echo "installation incomplete; inspect $APP and retained tree $APP_NEW; rerun $retry before restarting" >&2
    fi
  fi
}
trap cleanup_staging EXIT
trap 'exit 1' HUP INT TERM
install -d -m 0700 "$APP_NEW/bin" "$APP_NEW/lib" "$APP_NEW/licenses" "$APP_NEW/systemd"
cp -R "$SOURCE/bin/." "$APP_NEW/bin/"
cp -R "$SOURCE/lib/." "$APP_NEW/lib/"
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
if [ "$MODE" = setup ]; then
  # Guided setup names its own token file; otherwise the token goes here by hand.
  [ "$GUIDED" -eq 1 ] || [ -e "$CARILLON_HOME/credentials/discord-bot.token" ] ||
    install -m 0600 /dev/null "$CARILLON_HOME/credentials/discord-bot.token"
  chmod 0700 "$CARILLON_HOME" "$CARILLON_HOME/credentials" "$CARILLON_HOME/providers" \
    "$CARILLON_HOME/logs"
  chmod 0600 "$CARILLON_HOME/carillon.toml" "$CARILLON_HOME/providers"/*.toml \
    "$CARILLON_HOME/credentials"/*.token
fi

# Retain the previous application until registration succeeds. On any failure,
# leave the selected complete application in place and require a successful rerun.
"$APP/bin/carillon-register-services"
CLEANUP_APP_NEW=1

# The owner's stable command follows the published application path. Never
# replace another program or another live installation's entry point; a
# dangling link into a removed installation's app/bin is stale.
ENTRY_DIR=$HOME/.local/bin
ENTRY=$ENTRY_DIR/carillon
COMMAND=$APP/bin/carillon
install_entry_point() {
  if [ -L "$ENTRY" ]; then
    current=$(readlink -- "$ENTRY")
    [ "$current" != "$COMMAND" ] || return 0
    case "$current" in
      /*/app/bin/carillon)
        if [ -e "$ENTRY" ]; then
          echo "$ENTRY runs the installation at ${current%/app/bin/carillon}; left unchanged."
          return 1
        fi ;;
      *) echo "$ENTRY is not a Carillon entry point; left unchanged."; return 1 ;;
    esac
  elif [ -e "$ENTRY" ]; then
    echo "$ENTRY is not a Carillon entry point; left unchanged."
    return 1
  fi
  if ! { (umask 022 && mkdir -p -- "$ENTRY_DIR") && ln -sfn -- "$COMMAND" "$ENTRY"; }; then
    echo "could not create $ENTRY; the installation is complete without it."
    return 1
  fi
}
CARILLON=$COMMAND
if install_entry_point; then
  if [ "$(command -v carillon || true)" = "$ENTRY" ]; then
    CARILLON=carillon
  else
    echo "Installed $ENTRY, but $ENTRY_DIR is not on PATH or another carillon precedes it."
  fi
fi

if [ "$MODE" = update ]; then
  printf '%s\n' "Published this release at $APP and registered its user units; configuration and state were not changed."
  exit 0
fi
if [ "$GUIDED" -eq 1 ]; then
  # Starting takes the maintenance lock itself.
  exec 9>&-
  CARILLON_MAINTENANCE_LOCK=
  "$COMMAND" --home "$CARILLON_HOME" start
  cat <<EOF
Carillon is installed at $CARILLON_HOME and running as $OWNER's user services.
Everyday commands: $CARILLON status, $CARILLON logs, $CARILLON restart.
To start it at boot without logging in, an administrator can run: loginctl enable-linger $OWNER
EOF
  exit 0
fi
if grep -Fqx 'runtime_provider = "runtime-t3"' "$CARILLON_HOME/carillon.toml"; then
  RUNTIME_STEP="Set up T3 with: $CARILLON runtime-t3 configure --url URL --project PROJECT_ID --model INSTANCE/MODEL < t3-token-file"
else
  RUNTIME_STEP="Sign in to a model provider with: $APP/bin/carillon-pi --no-session --no-tools --no-approve
   In Pi, run /login, then /model and press Ctrl+S on the agent's model to save it."
fi
cat <<EOF
Carillon installed for $OWNER at $CARILLON_HOME and enabled for this user's service manager.
1. Put the Discord bot token in $CARILLON_HOME/credentials/discord-bot.token (mode 0600).
2. $RUNTIME_STEP
3. Start deliberately with: $CARILLON start
4. Inspect with: $CARILLON status   and   $CARILLON logs
Update later with: $CARILLON update /path/to/carillon-VERSION-linux-ARCH.tar.gz
For boot without an interactive login, an administrator may run: loginctl enable-linger $OWNER
EOF
