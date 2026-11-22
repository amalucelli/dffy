#!/bin/sh
# The `pi` on agent.rs's PATH. What it does comes from files a test put in
# $STUB/pi: `version` replaces what --version prints, `answer.jsonl` is the
# events a run prints, `stderr` what it prints on stderr and `status` its
# exit status. `--help` lists the recipe's flags but the one in `unknown`.
# A run writes its arguments to `args`, one a line, its directory to `cwd`
# and what it read on stdin to `stdin`.
d="$STUB/pi"
if [ "$1" = --help ]; then
  for flag in --mode --no-session --tools --no-mcp --no-approve --offline; do
    [ "$flag" = "$(cat "$d/unknown" 2>/dev/null)" ] || printf '  %s\n' "$flag"
  done
  exit 0
fi
case "$1" in
  --version)
    if [ -f "$d/version" ]; then cat "$d/version"; else echo '1.5.2'; fi
    exit 0 ;;
esac
printf '%s\n' "$@" > "$d/args"
pwd > "$d/cwd"
cat > "$d/stdin"
if [ -f "$d/stderr" ]; then cat "$d/stderr" >&2; fi
if [ -f "$d/answer.jsonl" ]; then cat "$d/answer.jsonl"; fi
if [ -f "$d/status" ]; then exit "$(cat "$d/status")"; fi
exit 0
