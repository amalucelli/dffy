#!/bin/sh
# The `claude` dffy-agent finds on the viewer's PATH. What it does comes
# from files a test put in $STUB: `answer.jsonl` is the stream a run prints
# and `status` its exit status, `logged-out` makes `auth status` exit 1,
# and `sleep` makes a run wait in a background `sleep` instead, its own pid
# and the sleep's written to `pids`. A run writes its arguments to `args`,
# one a line, so an empty one shows, and what it read on stdin to `stdin`.
d="$STUB"
case "$1" in
  --version)
    echo '2.1.329 (Claude Code)'
    exit 0 ;;
  auth)
    if [ -f "$d/logged-out" ]; then echo '{"loggedIn": false}'; exit 1; fi
    echo '{"loggedIn": true}'
    exit 0 ;;
esac
printf '%s\n' "$@" > "$d/args"
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
