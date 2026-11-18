#!/bin/sh
# Logs each call, `<args> after <lines printed>`, then runs the real git
# at $STUB_GIT.
echo "$* after $(wc -l < "$STUB/out" | tr -d ' ')" >> "$STUB/git"
exec "$STUB_GIT" "$@"
