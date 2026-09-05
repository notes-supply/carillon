#!/bin/sh
set -eu

[ "$(id -u)" -ne 0 ] || { echo "Carillon Pi login must run as the non-root owner" >&2; exit 1; }
: "${HOME:?HOME is required}"
LAUNCHER=$(readlink -f -- "$0")
BIN=$(dirname -- "$LAUNCHER")
CARILLON_HOME=$(CDPATH='' cd -- "$BIN/../.." && pwd)
export CARILLON_HOME
export PI_CODING_AGENT_DIR="$CARILLON_HOME/pi"
umask 077
exec "$BIN/pi" "$@"
