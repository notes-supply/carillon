#!/bin/sh
set -eu

text=${1:?text required}
headers=${2:?headers file required}
password='correct horse battery'
origin=$(printf '%s\n' "$password" | carillon web password | sed -n 's/.* Sign in at \(.*\)\.$/\1/p')
[ -n "$origin" ] || { printf 'carillon web password did not say where to sign in\n' >&2; exit 1; }
token=$(curl --fail --silent --max-time 120 -X POST -H 'content-type: application/json' -H "origin: $origin" \
  --data "{\"password\":\"$password\"}" "$origin/api/sign-in" |
  sed 's/.*"token":"\([^"]*\)".*/\1/')
(umask 077 && printf 'authorization: Bearer %s\n' "$token" >"$headers")
chat=$(cat /proc/sys/kernel/random/uuid)
event=m-$(cat /proc/sys/kernel/random/uuid)
curl --fail --silent --max-time 120 -X PUT -H "@$headers" -F "text=$text" \
  "$origin/api/conversations/assistant/$chat/messages/$event" >/dev/null
stream=$(mktemp -u)
mkfifo "$stream"
curl --silent --no-buffer --max-time 120 -H "@$headers" \
  "$origin/api/stream?conversations=1&agent_id=assistant&audience_id=$chat&turns=1" >"$stream" &
reader=$!
answered=$(grep -m 1 -F '"kind":"answered"' <"$stream") || true
kill "$reader" 2>/dev/null || true
rm -f "$stream"
case "$answered" in
  *"Local provider response 1: $text"*) ;;
  '') printf 'the web chat did not answer\n' >&2; exit 1 ;;
  *) printf 'the web chat answered something else: %s\n' "$answered" >&2; exit 1 ;;
esac
