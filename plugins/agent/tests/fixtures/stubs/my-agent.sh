#!/bin/sh
# A custom agent command on agent.rs's PATH: it writes its arguments to
# $STUB/my-agent/args, one a line, its directory to `cwd` and what it read
# on stdin to `stdin`, then prints `answer.txt` as its reply, or fails with
# `error` on stderr when that's there.
d="$STUB/my-agent"
printf '%s\n' "$@" > "$d/args"
pwd > "$d/cwd"
cat > "$d/stdin"
if [ -f "$d/error" ]; then cat "$d/error" >&2; exit 3; fi
cat "$d/answer.txt"
