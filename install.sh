#!/bin/sh
# Installs Carillon from its GitHub releases: the newest one, betas included,
# or the one --version names. It checks this machine, downloads the release
# and its checksums, verifies the download, and runs the release's own
# installer, which asks the rest at the terminal or takes its unattended
# options:
#
#   curl -fsSL https://raw.githubusercontent.com/notes-supply/carillon/main/install.sh | sh
#   curl -fsSL https://raw.githubusercontent.com/notes-supply/carillon/main/install.sh |
#     sh -s -- --version 0.1.0-beta.2 [installer options]
set -eu

REPOSITORY=notes-supply/carillon
# Where releases are found; a mirror or a test can name another.
RELEASES=${CARILLON_RELEASES:-https://github.com/$REPOSITORY/releases}
API=${CARILLON_RELEASES_API:-https://api.github.com/repos/$REPOSITORY/releases}

fail() {
  echo "carillon install: $*" >&2
  exit 1
}

VERSION=
if [ "${1:-}" = --version ]; then
  [ "$#" -ge 2 ] || fail "--version needs a version, such as 0.1.0-beta.2"
  VERSION=${2#v}
  shift 2
fi

[ "$(uname -s)" = Linux ] || fail "Carillon runs on Linux; this is $(uname -s)"
[ "$(uname -m)" = x86_64 ] || fail "Carillon's releases are built for x86_64; this machine is $(uname -m)"
[ "$(id -u)" -ne 0 ] || fail "run this as the user who will own Carillon, not as root"
glibc=$(getconf GNU_LIBC_VERSION 2>/dev/null || true)
case "$glibc" in
  'glibc '*) ;;
  *) fail "Carillon needs glibc 2.39 or newer, and this system's C library is not glibc" ;;
esac
glibc=${glibc#glibc }
major=${glibc%%.*}
minor=${glibc#*.}
minor=${minor%%.*}
[ "$major" -gt 2 ] || { [ "$major" -eq 2 ] && [ "$minor" -ge 39 ]; } ||
  fail "Carillon needs glibc 2.39 or newer; this system has $glibc"
for command in curl tar sha256sum flock systemctl; do
  command -v "$command" >/dev/null || fail "installing Carillon needs $command"
done
systemctl --user show-environment >/dev/null 2>&1 ||
  fail "no systemd user manager answers for $(id -un); log in to this account directly (not with su), or have an administrator run: loginctl enable-linger $(id -un)"

# Without its Discord options, the release's installer asks at the terminal.
unattended=0
for argument in "$@"; do
  case "$argument" in --guild-id | --bot-user-id | --user-id | --route) unattended=1 ;; esac
done
if [ "$unattended" -eq 0 ] && ! (: </dev/tty) 2>/dev/null; then
  fail "setup asks its questions at a terminal, and there is none; run this from a terminal, or give the installer's unattended options"
fi

if [ -z "$VERSION" ]; then
  # GitHub lists releases newest first, prereleases included.
  listing=$(curl -fsSL "$API?per_page=1") || fail "could not ask GitHub for Carillon's newest release"
  VERSION=$(printf '%s\n' "$listing" | sed -n 's/^ *"tag_name": *"v\([^"]*\)".*/\1/p' | head -n 1)
  [ -n "$VERSION" ] || fail "GitHub lists no Carillon release"
fi
case "$VERSION" in
  '' | *[!0-9A-Za-z.-]*) fail "$VERSION is not a Carillon version" ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/carillon-install.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
trap 'exit 1' HUP INT TERM
archive=carillon-$VERSION-linux-x86_64.tar.gz
echo "Downloading Carillon $VERSION"
curl -fsSL -o "$work/$archive" "$RELEASES/download/v$VERSION/$archive" ||
  fail "could not download $archive"
curl -fsSL -o "$work/SHA256SUMS" "$RELEASES/download/v$VERSION/SHA256SUMS" ||
  fail "could not download the checksums of Carillon $VERSION"
(cd "$work" && awk -v name="$archive" '$2 == name' SHA256SUMS | sha256sum -c --status) ||
  fail "$archive does not match its published checksum; nothing was installed"
tar -xzf "$work/$archive" -C "$work"
if [ "$unattended" -eq 1 ]; then
  "$work/carillon-$VERSION/install.sh" "$@"
else
  # Piped into sh, this script's standard input is the download, not the
  # person: the installer and the programs it runs get the terminal.
  "$work/carillon-$VERSION/install.sh" "$@" </dev/tty
fi
