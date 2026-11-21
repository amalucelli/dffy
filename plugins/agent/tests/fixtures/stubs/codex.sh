#!/bin/sh
# The `codex` on agent.rs's PATH. What it does comes from files a test put
# in $STUB/codex: `version` is the version --version prints after
# `codex-cli`, `logged-out` makes `login status` exit 1, `slow` makes both
# wait that many seconds first, `answer.jsonl` is the events a run prints, `stderr` what it prints on stderr and `status`
# its exit status. `exec --help` lists the recipe's flags but the one in
# `unknown`. A run writes its arguments to `args`, one a line, its
# directory to `cwd` and what it read on stdin to `stdin`.
d="$STUB/codex"
if [ "$1 $2" = "exec --help" ]; then
  for flag in --json --sandbox --ephemeral -c -C; do
    [ "$flag" = "$(cat "$d/unknown" 2>/dev/null)" ] || printf '      %s <value>\n' "$flag"
  done
  exit 0
fi
case "$1" in
  --version|login)
    if [ -f "$d/slow" ]; then sleep "$(cat "$d/slow")"; fi ;;
esac
case "$1" in
  --version)
    printf 'codex-cli %s\n' "$(cat "$d/version" 2>/dev/null || echo 0.59.0)"
    exit 0 ;;
  login)
    [ "$2" = status ] || { echo "codex stub: login $2" >&2; exit 2; }
    if [ -f "$d/logged-out" ]; then echo 'Not logged in' >&2; exit 1; fi
    echo 'Logged in using ChatGPT'; exit 0 ;;
esac
printf '%s\n' "$@" > "$d/args"
pwd > "$d/cwd"
cat > "$d/stdin"
if [ -f "$d/stderr" ]; then cat "$d/stderr" >&2; fi
if [ -f "$d/answer.jsonl" ]; then cat "$d/answer.jsonl"; fi
if [ -f "$d/status" ]; then exit "$(cat "$d/status")"; fi
exit 0
