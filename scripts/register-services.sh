#!/bin/sh
set -eu
umask 077

# Keep recovery copies after a publication attempt; a successful rerun is the
# recovery procedure. Application selection and systemd are not one transaction.
UNIT_STAGE=
PUBLICATION_STARTED=0
cleanup_staging() {
  result=$?
  if [ -n "$UNIT_STAGE" ]; then
    if [ "$result" -ne 0 ] && [ "$PUBLICATION_STARTED" -eq 1 ]; then
      echo "service registration failed; retained backups at $UNIT_STAGE; rerun registration before restarting" >&2
    else
      rm -rf -- "$UNIT_STAGE"
    fi
  fi
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
SELF=$(realpath -e -- "$0")
CARILLON_HOME=$(dirname -- "$(dirname -- "$(dirname -- "$SELF")")")
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
# filesystem so each definition is published as a complete file.
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
  mv -T -- "$UNIT_STAGE/new/$name" "$UNIT_DIR/$name"
done
# A barrier failure does not undo a published definition.
sync -f -- "$UNIT_DIR"

if [ "${CARILLON_SKIP_SYSTEMD_RELOAD:-0}" != "1" ]; then
  systemctl --user daemon-reload
  systemctl --user enable carillon.target
  # Enablement creates the wants symlink after definition publication.
  sync -f -- "$UNIT_DIR"
fi

printf '%s\n' "Carillon user units registered for $OWNER at $CARILLON_HOME; services were not started."
