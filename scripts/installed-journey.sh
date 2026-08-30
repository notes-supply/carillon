#!/bin/sh
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
RELEASE=${1:-"$ROOT/target/package/carillon-0.0.0"}
IMAGE=${CARILLON_TEST_IMAGE:-docker.io/library/archlinux:base}
BASE_PORT=${CARILLON_TEST_BASE_PORT:-44800}

if [ ! -x "$RELEASE/install.sh" ]; then
  printf 'release stage not found: %s\n' "$RELEASE" >&2
  exit 1
fi

podman run --rm --network host --env CARILLON_TEST_BASE_PORT="$BASE_PORT" -v "$RELEASE:/release:ro" "$IMAGE" /bin/sh -euxc '
  pacman -Sy --noconfirm curl shadow
  CARILLON_SKIP_SYSTEMD_RELOAD=1 /release/install.sh --guild-id 100 --bot-user-id 201 --user-id 200 --route 300:always:channel --base-port "$CARILLON_TEST_BASE_PORT"
  /opt/carillon/bin/carillon component --config /var/lib/carillon/carillon.toml guild-command enable
  /opt/carillon/bin/carillon component --config /var/lib/carillon/carillon.toml echo-capability enable
  /opt/carillon/bin/carillon enable-demo-resources --config /var/lib/carillon/carillon.toml

  install -d -m 0755 /run/carillon-test
  install -o carillon-runtime -g carillon-runtime -m 0600 /var/lib/carillon/credentials/runtime-pi.token /run/carillon-test/runtime.token
  install -o carillon-discord -g carillon-discord -m 0600 /var/lib/carillon/credentials/discord-private.token /run/carillon-test/discord.token
  install -o carillon-extension -g carillon-extension -m 0600 /var/lib/carillon/credentials/guild-command.token /run/carillon-test/extension.token
  install -o carillon-capability -g carillon-capability -m 0600 /var/lib/carillon/credentials/echo-capability.token /run/carillon-test/capability.token

  runuser -u carillon-runtime -g carillon-runtime -G carillon -- /opt/carillon/bin/carillon-runtime-pi --config /var/lib/carillon/carillon.toml --token-file /run/carillon-test/runtime.token --state-dir /var/lib/pi-carillon --deterministic >/tmp/runtime.log 2>&1 & runtime_pid=$!
  runuser -u carillon-extension -g carillon-extension -G carillon -- /opt/carillon/bin/carillon interface-extension --config /var/lib/carillon/carillon.toml --token-file /run/carillon-test/extension.token >/tmp/extension.log 2>&1 & extension_pid=$!
  runuser -u carillon-capability -g carillon-capability -G carillon -- /opt/carillon/bin/carillon capability --config /var/lib/carillon/carillon.toml --token-file /run/carillon-test/capability.token >/tmp/capability.log 2>&1 & capability_pid=$!
  runuser -u carillon-core -g carillon-core -G carillon -- /opt/carillon/bin/carillon core --config /var/lib/carillon/carillon.toml >/tmp/core.log 2>&1 & core_pid=$!
  runuser -u carillon-discord -g carillon-discord -G carillon -- /opt/carillon/bin/carillon discord --config /var/lib/carillon/carillon.toml --token-file /run/carillon-test/discord.token --deterministic >/tmp/discord.log 2>&1 & discord_pid=$!
  trap "kill $discord_pid $core_pid $capability_pid $extension_pid $runtime_pid 2>/dev/null || true" EXIT INT TERM

  for offset in 0 1 2 3 4; do
    port=$((CARILLON_TEST_BASE_PORT + offset))
    attempt=0
    until curl --fail --silent "http://127.0.0.1:$port/health" >/dev/null; do
      attempt=$((attempt + 1))
      if [ "$attempt" -ge 100 ]; then
        cat /tmp/runtime.log /tmp/extension.log /tmp/capability.log /tmp/core.log /tmp/discord.log >&2
        exit 1
      fi
      sleep 0.05
    done
  done

  /opt/carillon/bin/carillon model --config /var/lib/carillon/carillon.toml deterministic/steady >/dev/null
  discord_port=$((CARILLON_TEST_BASE_PORT + 1))
  response=$(curl --fail --silent --show-error -X POST -H "content-type: application/json" --data "{\"event_id\":\"installed-event\",\"guild_id\":\"100\",\"user_id\":\"200\",\"channel_id\":\"300\",\"content\":\"installed identity turn\"}" "http://127.0.0.1:$discord_port/v1/simulate")
  case "$response" in *"Local provider response 1"*"processing"*"delivered"*) ;; *) printf "%s\n" "$response" >&2; exit 1;; esac

  recent=$(/opt/carillon/bin/carillon recent --config /var/lib/carillon/carillon.toml --event-id installed-event)
  case "$recent" in *installed-event*) ;; *) printf "%s\n" "$recent" >&2; exit 1;; esac

  CARILLON_SKIP_SYSTEMD_RELOAD=1 /release/install.sh --guild-id 100 --bot-user-id 201 --user-id 200 --route 300:always:channel --base-port "$CARILLON_TEST_BASE_PORT" >/dev/null
  grep -q "typing = true" /var/lib/carillon/carillon.toml
  grep -q "reactions = true" /var/lib/carillon/carillon.toml
  grep -q "processing_emoji = " /var/lib/carillon/carillon.toml
  grep -q "activation = \"always\"" /var/lib/carillon/carillon.toml
  grep -q "conversation = \"channel\"" /var/lib/carillon/carillon.toml
  session_owner=$(stat -c %U /var/lib/carillon/agents/default/sessions/*.json)
  [ "$session_owner" = carillon-runtime ]
  snapshot_owner=$(stat -c %U /var/lib/carillon/state/resource-snapshots/*)
  snapshot_group=$(stat -c %G /var/lib/carillon/state/resource-snapshots/*)
  [ "$snapshot_owner" = carillon-core ]
  [ "$snapshot_group" = carillon ]

  printf "%s\n" "installed service identities completed an ordinary turn"
'
