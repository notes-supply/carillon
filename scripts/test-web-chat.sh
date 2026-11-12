#!/bin/sh
set -eu

text=${1:?text required}
headers=${2:?headers file required}
link=$(carillon web open --print)
origin=${link%%/#sign-in=*}
token=$(curl --fail --silent -X POST -H 'content-type: application/json' -H "origin: $origin" \
  --data "{\"code\":\"${link#*#sign-in=}\"}" "$origin/api/sign-in" |
  sed 's/.*"token":"\([^"]*\)".*/\1/')
(umask 077 && printf 'authorization: Bearer %s\n' "$token" >"$headers")
chat=$(cat /proc/sys/kernel/random/uuid)
event=m-$(cat /proc/sys/kernel/random/uuid)
curl --fail --silent -X PUT -H "@$headers" -F "text=$text" \
  "$origin/api/conversations/$chat/messages/$event?agent=assistant" >/dev/null
attempt=0
while :; do
  turns=$(curl --fail --silent -H "@$headers" "$origin/api/conversations/$chat/turns")
  case "$turns" in *'"state":"answered"'*) break ;; esac
  attempt=$((attempt + 1))
  [ "$attempt" -lt 300 ] || { printf 'the web chat did not answer: %s\n' "$turns" >&2; exit 1; }
  sleep 0.1
done
case "$turns" in
  *"Local provider response 1: $text"*) ;;
  *) printf 'the web chat answered something else: %s\n' "$turns" >&2; exit 1 ;;
esac
