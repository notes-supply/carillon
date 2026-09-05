#!/bin/sh
set -eu
umask 077

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
UNIT_STAGE=
cleanup_staging() {
  [ -z "$UNIT_STAGE" ] || rm -rf -- "$UNIT_STAGE"
}
trap cleanup_staging EXIT
trap 'exit 1' HUP INT TERM

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
for template in "$SOURCE"/systemd/*; do
  name=$(basename "$template")
  if ! mv -T -- "$UNIT_STAGE/new/$name" "$UNIT_DIR/$name"; then
    echo "failed to publish unit: $UNIT_DIR/$name; restoring previous definitions" >&2
    restore_failed=0
    for restore_template in "$SOURCE"/systemd/*; do
      restore_name=$(basename "$restore_template")
      # A missing staged definition has already been published.
      [ ! -e "$UNIT_STAGE/new/$restore_name" ] || continue
      if [ -e "$UNIT_STAGE/old/$restore_name" ] || [ -L "$UNIT_STAGE/old/$restore_name" ]; then
        mv -T -- "$UNIT_STAGE/old/$restore_name" "$UNIT_DIR/$restore_name" || restore_failed=1
      else
        rm -f -- "$UNIT_DIR/$restore_name" || restore_failed=1
      fi
    done
    if [ "$restore_failed" -ne 0 ]; then
      echo "unit restoration failed; retained backups at $UNIT_STAGE" >&2
      UNIT_STAGE=
    fi
    exit 1
  fi
done

if [ "${CARILLON_SKIP_SYSTEMD_RELOAD:-0}" != "1" ]; then
  systemctl --user daemon-reload
  systemctl --user enable carillon.target
fi

printf '%s\n' "Carillon user units registered for $OWNER at $CARILLON_HOME; services were not started."
