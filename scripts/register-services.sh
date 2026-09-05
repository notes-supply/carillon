#!/bin/sh
set -eu
umask 077

# Exit 10 means registration failed with previous unit definitions intact.
# Other failures are ambiguous or occur after publication; callers must not
# select an older application on that evidence alone.
UNIT_STAGE=
PUBLICATION_STARTED=0
UNITS_PUBLISHED=0
cleanup_staging() {
  result=$?
  if [ "$result" -ne 0 ] && [ "$UNITS_PUBLISHED" -eq 0 ]; then
    result=10
    if [ "$PUBLICATION_STARTED" -eq 1 ]; then
      for template in "$SOURCE"/systemd/*; do
        name=$(basename "$template")
        # A missing staged definition has already been published.
        [ ! -e "$UNIT_STAGE/new/$name" ] || continue
        if [ -e "$UNIT_STAGE/old/$name" ] || [ -L "$UNIT_STAGE/old/$name" ]; then
          # Keep the backup until restoration itself is durably confirmed.
          if cp -Pp -- "$UNIT_STAGE/old/$name" "$UNIT_STAGE/new/$name"; then
            mv -T -- "$UNIT_STAGE/new/$name" "$UNIT_DIR/$name" || result=1
          else
            result=1
          fi
        else
          rm -f -- "$UNIT_DIR/$name" || result=1
        fi
      done
      sync -f -- "$UNIT_DIR" || result=1
      if [ "$result" -ne 10 ]; then
        echo "unit restoration unconfirmed; retained backups at $UNIT_STAGE" >&2
        UNIT_STAGE=
      fi
    fi
  elif [ "$result" -ne 0 ] && [ -n "$UNIT_STAGE" ]; then
    echo "unit publication or registration unconfirmed; retained backups at $UNIT_STAGE; rerun registration before restarting" >&2
    UNIT_STAGE=
  fi
  [ -z "$UNIT_STAGE" ] || rm -rf -- "$UNIT_STAGE"
  exit "$result"
}
trap cleanup_staging EXIT
trap 'exit 1' HUP INT TERM

# Registration belongs to the bundled release, not the provider-neutral restore.
# Resolve this installed helper's location so relocation cannot retain an old root.
[ "$#" -eq 0 ] || { echo "usage: carillon-register-services" >&2; exit 2; }
[ "$(id -u)" -ne 0 ] || { echo "service registration must run as the non-root owner" >&2; exit 1; }
[ -n "${HOME:-}" ] && [ -d "$HOME" ] || { echo "HOME must name the owner's existing home directory" >&2; exit 1; }
[ "$(stat -c %u "$HOME")" -eq "$(id -u)" ] || { echo "HOME is not owned by the service owner" >&2; exit 1; }
CARILLON_HOME=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd -P)
SOURCE=$CARILLON_HOME/app
OWNER=$(id -un)
OWNER_SHELL=$(getent passwd "$OWNER" | cut -d: -f7)
[ -n "$OWNER_SHELL" ] && [ -x "$OWNER_SHELL" ] || { echo "cannot resolve the owner's login shell" >&2; exit 1; }
OWNER_PATH=${PATH:-/usr/local/bin:/usr/bin:/bin}
UNIT_DIR=${XDG_CONFIG_HOME:-"$HOME/.config"}/systemd/user
case "$HOME:$CARILLON_HOME:$OWNER_PATH:$OWNER_SHELL:$UNIT_DIR" in
  *'
'*) echo "service paths must not contain newlines" >&2; exit 1 ;;
esac
case "$HOME" in /*) ;; *) echo "HOME must be an absolute path" >&2; exit 1 ;; esac
case "$UNIT_DIR" in /*) ;; *) echo "XDG_CONFIG_HOME must be an absolute path" >&2; exit 1 ;; esac
[ -f "$CARILLON_HOME/carillon.toml" ] || { echo "installed configuration is missing" >&2; exit 1; }
# This directory is shared with unrelated user services. Preserve its mode.
mkdir -p "$UNIT_DIR"

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
# Render and set modes before touching any live unit. Keep staging on the same
# filesystem so publication and restoration each replace a definition atomically.
UNIT_STAGE=$(mktemp -d "$UNIT_DIR/.carillon-units.XXXXXX")
mkdir "$UNIT_STAGE/new" "$UNIT_STAGE/old"
for template in "$SOURCE"/systemd/*; do
  name=$(basename "$template")
  unit=$UNIT_DIR/$name
  sed \
    -e "s|@OWNER_HOME@|$HOME_UNIT|g" \
    -e "s|@OWNER@|$OWNER_UNIT|g" \
    -e "s|@CARILLON_HOME@|$ROOT_UNIT|g" \
    -e "s|@OWNER_SHELL@|$SHELL_UNIT|g" \
    -e "s|@OWNER_PATH@|$PATH_UNIT|g" \
    "$template" >"$UNIT_STAGE/new/$name"
  chmod 0600 "$UNIT_STAGE/new/$name"
  if [ -e "$unit" ] || [ -L "$unit" ]; then
    cp -Pp -- "$unit" "$UNIT_STAGE/old/$name"
  fi
done
# XDG_CONFIG_HOME may be on a different filesystem from the application. Persist
# rendered definitions, modes and recovery copies before the first rename.
sync -f -- "$UNIT_STAGE"
PUBLICATION_STARTED=1
for template in "$SOURCE"/systemd/*; do
  name=$(basename "$template")
  if ! mv -T -- "$UNIT_STAGE/new/$name" "$UNIT_DIR/$name"; then
    echo "failed to publish unit: $UNIT_DIR/$name; restoring previous definitions" >&2
    exit 1
  fi
done
UNITS_PUBLISHED=1
# A complete publication is not rolled back merely because its barrier fails.
sync -f -- "$UNIT_DIR"

if [ "${CARILLON_SKIP_SYSTEMD_RELOAD:-0}" != "1" ]; then
  systemctl --user daemon-reload
  systemctl --user enable carillon.target
fi

printf '%s\n' "Carillon user units registered for $OWNER at $CARILLON_HOME; services were not started."
