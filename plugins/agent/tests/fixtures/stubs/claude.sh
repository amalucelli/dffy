#!/bin/sh
# The `claude` on agent.rs's PATH. What it does comes from files a test put
# in $STUB/claude: `version` replaces what --version prints, `logged-out`
# makes `auth status` exit 1, `slow` makes both wait that many seconds
# first, `answer.jsonl` is the stream a run prints and
# `status` its exit status, and `sleep` makes a run wait in a background
# `sleep` instead, its own pid and the sleep's written to `pids`. A run
# writes its arguments to `args`, one a line, so an empty one shows, its
# directory to `cwd` and what it read on stdin to `stdin`. `--help` lists
# the recipe's flags but the one in `unknown`.
d="$STUB/claude"
if [ "$1" = --help ]; then
  for flag in -p --verbose --output-format --tools --disallowedTools --strict-mcp-config \
    --setting-sources --no-session-persistence --permission-mode; do
    [ "$flag" = "$(cat "$d/unknown" 2>/dev/null)" ] || printf '  %s, --other\n' "$flag"
  done
  exit 0
fi
case "$1" in
  --version|auth)
    if [ -f "$d/slow" ]; then sleep "$(cat "$d/slow")"; fi ;;
esac
case "$1" in
  --version)
    if [ -f "$d/version" ]; then cat "$d/version"; else echo '2.1.329 (Claude Code)'; fi
    exit 0 ;;
  auth)
    [ "$2" = status ] || { echo "claude stub: auth $2" >&2; exit 2; }
    if [ -f "$d/logged-out" ]; then echo '{"loggedIn": false}'; exit 1; fi
    echo '{"loggedIn": true}'; exit 0 ;;
esac
printf '%s\n' "$@" > "$d/args"
pwd > "$d/cwd"
cat > "$d/stdin"
if [ -f "$d/sleep" ]; then
  sleep 30 &
  echo "$$ $!" > "$d/pids"
  wait
  exit 0
fi
cat "$d/answer.jsonl"
if [ -f "$d/status" ]; then exit "$(cat "$d/status")"; fi
exit 0
