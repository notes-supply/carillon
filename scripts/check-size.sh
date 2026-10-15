#!/usr/bin/env bash
# Keeps source files under LIMIT lines. Files already longer are listed in
# lint/size-budget.txt at their current length, which may only shrink:
# `--update` lowers entries to what their files shrank to and drops the rest,
# but never raises an entry or adds a file.
set -euo pipefail
cd "$(dirname "$0")/.."

readonly LIMIT=1000 BUDGET=lint/size-budget.txt
update=false
[ "${1-}" = --update ] && update=true
next=$(mktemp)
trap 'rm -f "$next"' EXIT

status=0
git ls-files -z -- '*.rs' '*.ts' '*.mjs' '*.sh' | xargs -0 wc -l | awk '$2 != "total"' |
  awk -v limit="$LIMIT" -v budget="$BUDGET" -v out="$next" -v update="$update" '
    FILENAME == budget { allowed[$2] = $1; next }
    {
      n = $1; path = $2
      if (path in allowed) {
        if (n > allowed[path]) { print path ": " n " lines, over its budget of " allowed[path]; failed = 1 }
        kept = n < allowed[path] ? n : allowed[path]
        if (kept > limit) { keep[path] = kept; print kept, path > out }
      } else if (n > limit) { print path ": " n " lines, over the " limit "-line limit; split it"; failed = 1 }
    }
    END {
      for (path in allowed) if (!(path in keep) || keep[path] != allowed[path]) stale = 1
      if (stale && update != "true") { print budget " is stale; run scripts/check-size.sh --update"; failed = 1 }
      exit failed
    }
  ' "$BUDGET" - || status=$?

if $update; then sort -k2 "$next" >"$BUDGET"; fi
exit "$status"
