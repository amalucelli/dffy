#!/bin/sh
# Logs each call, `<args> after <lines printed>`, the plugin's answer to
# `initialize` and what it said to write not counted, then runs the real
# git at $STUB_GIT.
n=$(grep -cv '"comments/apply"' "$STUB/out")
echo "$* after $((n > 0 ? n - 1 : 0))" >> "$STUB/git"
exec "$STUB_GIT" "$@"
