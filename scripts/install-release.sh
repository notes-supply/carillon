#!/bin/sh
set -eu

usage() {
  echo "usage: install.sh --guild-id ID --user-id ID [--user-id ID ...] [--base-port PORT]" >&2
  exit 2
}

GUILD_ID=
USER_IDS=
BASE_PORT=43800
while [ "$#" -gt 0 ]; do
  case "$1" in
    --guild-id) [ "$#" -ge 2 ] || usage; GUILD_ID=$2; shift 2 ;;
    --user-id) [ "$#" -ge 2 ] || usage; USER_IDS="$USER_IDS --user-id $2"; shift 2 ;;
    --base-port) [ "$#" -ge 2 ] || usage; BASE_PORT=$2; shift 2 ;;
    *) usage ;;
  esac
done
[ -n "$GUILD_ID" ] || usage
[ -n "$USER_IDS" ] || usage
[ "$(id -u)" -eq 0 ] || { echo "installer must run as root" >&2; exit 1; }

SOURCE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
getent group carillon >/dev/null 2>&1 || groupadd --system carillon
for account in carillon-core carillon-discord carillon-runtime carillon-extension carillon-capability; do
  getent group "$account" >/dev/null 2>&1 || groupadd --system "$account"
  if id "$account" >/dev/null 2>&1; then
    usermod --gid "$account" --append --groups carillon "$account"
  else
    useradd --system --home-dir /nonexistent --shell /usr/bin/nologin --gid "$account" --groups carillon "$account"
  fi
done

install -d -m 0755 /opt/carillon/bin /etc/carillon /var/lib/carillon
install -m 0755 "$SOURCE/bin/carillon" /opt/carillon/bin/carillon
install -m 0755 "$SOURCE/bin/carillon-runtime-pi" /opt/carillon/bin/carillon-runtime-pi
install -m 0644 "$SOURCE"/systemd/*.service /etc/systemd/system/

if [ ! -f /var/lib/carillon/carillon.toml ]; then
  # USER_IDS intentionally expands into repeated, validated CLI arguments.
  # shellcheck disable=SC2086
  /opt/carillon/bin/carillon init --root /var/lib/carillon --guild-id "$GUILD_ID" $USER_IDS --base-port "$BASE_PORT"
fi

if [ ! -e /etc/carillon/discord.token ]; then install -m 0600 /dev/null /etc/carillon/discord.token; fi
if [ ! -e /etc/carillon/provider.key ]; then install -m 0600 /dev/null /etc/carillon/provider.key; fi
chown -R root:carillon /var/lib/carillon
chmod 2750 /var/lib/carillon /var/lib/carillon/agents /var/lib/carillon/credentials
chmod 2750 /var/lib/carillon/agents/default /var/lib/carillon/agents/default/extensions /var/lib/carillon/agents/default/skills
chown -R carillon-runtime:carillon-runtime /var/lib/carillon/agents/default/sessions
chmod 0750 /var/lib/carillon/agents/default/sessions
chmod 0640 /var/lib/carillon/carillon.toml /var/lib/carillon/agents/default/SYSTEM.md /var/lib/carillon/agents/default/AGENTS.md /var/lib/carillon/agents/default/settings.json
chown -R carillon-core:carillon-core /var/lib/carillon/state
install -d -o carillon-core -g carillon -m 2750 /var/lib/carillon/state/resource-snapshots
chown -R carillon-runtime:carillon-runtime /var/lib/carillon/state/components/runtime-pi
chown -R carillon-capability:carillon-capability /var/lib/carillon/state/components/echo-capability
chown root:carillon-core /var/lib/carillon/credentials/*.token
chmod 0640 /var/lib/carillon/credentials/*.token
if [ "${CARILLON_SKIP_SYSTEMD_RELOAD:-0}" != "1" ]; then systemctl daemon-reload; fi

cat <<'EOF'
Carillon installed but not started.
1. Put the Discord bot token in /etc/carillon/discord.token (mode 0600).
2. Put an Anthropic API key in /etc/carillon/provider.key and select a model in /var/lib/carillon/agents/default/settings.json.
3. Start the five carillon-*.service units deliberately.
EOF
