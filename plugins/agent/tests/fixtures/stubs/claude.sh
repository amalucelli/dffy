#!/bin/sh
# The `claude` on agent.rs's PATH. What it does comes from files a test put
# in $STUB/claude: `version` replaces what --version prints, `logged-out`
# makes `auth status` exit 1, `answer.jsonl` is the stream a run prints and
# `status` its exit status, and `sleep` makes a run wait in a background
# `sleep` instead, its own pid and the sleep's written to `pids`. A run
# writes its arguments to `args`, one a line, so an empty one shows, its
# directory to `cwd` and what it read on stdin to `stdin`.
d="$STUB/claude"
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
