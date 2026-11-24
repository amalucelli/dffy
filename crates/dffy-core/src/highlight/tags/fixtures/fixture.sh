#!/usr/bin/env bash
set -euo pipefail

LIMIT=10
readonly ROOT="$(dirname "$0")"
export PATH="$ROOT/bin:$PATH"

usage() {
  local code=1
  echo "usage" >&2
}

function build {
  inner() { :; }
  make all
}

if [ -n "${DEBUG:-}" ]; then
  debug() { echo "$@"; }
fi

cat <<EOF2
not() { a function; }
EOF2
