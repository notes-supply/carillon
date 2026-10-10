#!/bin/sh
# The public installer against a local stand-in for GitHub's releases: host
# checks, the newest release with betas included, a pinned version, the
# checksum check, the installer's options and the terminal requirement.
set -eu

ROOT=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)
TEST_ROOT=$(mktemp -d)
SERVER=
cleanup() {
  [ -z "$SERVER" ] || kill "$SERVER" 2>/dev/null || true
  rm -rf -- "$TEST_ROOT"
}
trap cleanup EXIT
fail() {
  echo "bootstrap test: $*" >&2
  exit 1
}

# Releases: a beta newest, an older stable one, and one whose checksum lies.
RELEASES=$TEST_ROOT/releases
release() {
  version=$1
  stage=$TEST_ROOT/stage/carillon-$version
  mkdir -p "$stage" "$RELEASES/download/v$version"
  cat >"$stage/install.sh" <<'EOF'
#!/bin/sh
{
  printf 'version %s\n' "$(basename -- "$(dirname -- "$0")")"
  printf 'arguments %s\n' "$*"
} >>"$RECORD"
EOF
  chmod 0755 "$stage/install.sh"
  archive=carillon-$version-linux-x86_64.tar.gz
  tar -C "$TEST_ROOT/stage" -czf "$RELEASES/download/v$version/$archive" "carillon-$version"
  (cd "$RELEASES/download/v$version" && sha256sum "$archive" >SHA256SUMS)
}
release 0.2.0-beta.1
release 0.1.0
release 0.0.9
printf '%064d  carillon-0.0.9-linux-x86_64.tar.gz\n' 0 >"$RELEASES/download/v0.0.9/SHA256SUMS"
printf '%s\n' '[' '  {' '    "tag_name": "v0.2.0-beta.1",' '    "prerelease": true' '  },' \
  '  {' '    "tag_name": "v0.1.0",' '    "prerelease": false' '  }' ']' >"$RELEASES/api.json"
# shellcheck disable=SC2016 # JavaScript, not shell
node -e '
const { createServer } = require("node:http");
const { readFile, appendFile } = require("node:fs/promises");
const { join } = require("node:path");
const [root, log] = process.argv.slice(1);
const server = createServer(async (request, response) => {
  await appendFile(log, `${request.url}\n`);
  const path = request.url.startsWith("/api?") ? "api.json" : request.url.slice(1);
  try {
    response.end(await readFile(join(root, path)));
  } catch {
    response.writeHead(404).end();
  }
});
server.listen(0, "127.0.0.1", () => console.log(server.address().port));
' "$RELEASES" "$TEST_ROOT/requests" >"$TEST_ROOT/port" &
SERVER=$!
for _ in 1 2 3 4 5 6 7 8 9 10; do [ -s "$TEST_ROOT/port" ] && break; sleep 0.2; done
PORT=$(cat "$TEST_ROOT/port")
[ -n "$PORT" ] || fail "the release stand-in did not start"

# This machine as the installer sees it.
COMMANDS=$TEST_ROOT/commands
mkdir "$COMMANDS"
cat >"$COMMANDS/uname" <<'EOF'
#!/bin/sh
case "$1" in -s) echo Linux ;; -m) echo "${TEST_ARCH:-x86_64}" ;; esac
EOF
cat >"$COMMANDS/getconf" <<'EOF'
#!/bin/sh
echo "glibc ${TEST_GLIBC:-2.43}"
EOF
cat >"$COMMANDS/systemctl" <<'EOF'
#!/bin/sh
[ "$*" = "--user show-environment" ]
EOF
chmod 0755 "$COMMANDS"/*
RECORD=$TEST_ROOT/record
install() {
  env PATH="$COMMANDS:$PATH" TMPDIR="$TEST_ROOT" RECORD="$RECORD" \
    CARILLON_RELEASES="http://127.0.0.1:$PORT" CARILLON_RELEASES_API="http://127.0.0.1:$PORT/api" \
    "$@"
}
OPTIONS='--guild-id 100 --bot-user-id 201 --user-id 200 --route 300:always:thread'

refused() {
  expected=$1
  shift
  : >"$TEST_ROOT/requests"
  if install "$@" >"$TEST_ROOT/out" 2>&1; then
    cat "$TEST_ROOT/out" >&2
    fail "accepted: $*"
  fi
  grep -F "$expected" "$TEST_ROOT/out" >/dev/null || { cat "$TEST_ROOT/out" >&2; fail "expected: $expected"; }
}
# shellcheck disable=SC2086 # OPTIONS are separate installer arguments
{
  refused 'built for x86_64; this machine is aarch64' env TEST_ARCH=aarch64 sh "$ROOT/install.sh" $OPTIONS
  refused 'needs glibc 2.39 or newer; this system has 2.36' env TEST_GLIBC=2.36 sh "$ROOT/install.sh" $OPTIONS
  [ ! -s "$TEST_ROOT/requests" ] || fail "an unsupported host downloaded something"
  # Without its options the installer asks at a terminal; with none, it says so.
  refused 'setup asks its questions at a terminal' setsid -w sh "$ROOT/install.sh" </dev/null
  refused 'does not match its published checksum; nothing was installed' \
    sh "$ROOT/install.sh" --version 0.0.9 $OPTIONS
  [ ! -e "$RECORD" ] || fail "a download with the wrong checksum was installed"

  # The newest release, a beta, unless a version is named; options pass through.
  install sh "$ROOT/install.sh" $OPTIONS >/dev/null
  install sh "$ROOT/install.sh" --version v0.1.0 $OPTIONS >/dev/null
}
printf '%s\n' 'version carillon-0.2.0-beta.1' "arguments $OPTIONS" \
  'version carillon-0.1.0' "arguments $OPTIONS" | cmp - "$RECORD"
[ -z "$(find "$TEST_ROOT" -maxdepth 1 -name 'carillon-install.*')" ] || fail "downloads were left behind"
printf '%s\n' 'bootstrap passed: host checks, newest beta, pinned version, checksum and terminal'
