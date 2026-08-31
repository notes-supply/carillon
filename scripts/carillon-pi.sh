#!/bin/sh
set -eu

[ "$(id -u)" -ne 0 ] || { echo "Carillon Pi login must run as the non-root owner" >&2; exit 1; }
: "${HOME:?HOME is required}"
CARILLON_HOME=${CARILLON_HOME:-"$HOME/.carillon"}
export CARILLON_HOME
export PI_CODING_AGENT_DIR="$CARILLON_HOME/pi"
umask 077
exec "$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)/pi" "$@"
