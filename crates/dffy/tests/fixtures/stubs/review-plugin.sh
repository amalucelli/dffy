#!/bin/sh
# A review plugin for dffy's tests, installed as dffy-review-test. Each
# `describe` adds a line to `described` beside it: tty or notty, whether it
# had a terminal. Each other verb adds to `calls` its arguments, working
# directory, tty or notty, and what dffy set in its environment.
#
# A call then answers from files beside it, which the test writes, named
# by a stem: the verb, or `<verb>.<flag>` when that call has `--<flag>` and
# `<verb>.<flag>.out` exists.
#   <stem>.out   printed; without it, the verb is unknown (exit 2)
#   <stem>.code  the exit status, 0 without it
#   <stem>.gate  the call waits while it exists, before printing
#   <stem>.hold  the call keeps running after printing, with a child whose
#                pid goes in `pid`
# The call's arguments go in `<stem>.args`, and the file after
# `--summary-file` is copied to `summary`.
d=$(dirname "$0")
if ( : < /dev/tty ) 2>/dev/null; then tty='tty'; else tty='notty'; fi
if [ "$1" = describe ]; then
  echo "$tty" >> "$d/described"
else
  {
    echo "$*"
    pwd -P
    echo "$tty"
    echo "protocol=$DFFY_REVIEW_PROTOCOL comments=$DFFY_COMMENTS dffy=$DFFY"
  } >> "$d/calls"
fi
stem=$1
prev=
for arg in "$@"; do
  if [ "$prev" = --summary-file ]; then cp "$arg" "$d/summary"; fi
  case "$arg" in
    --*) if [ -f "$d/$1.${arg#--}.out" ]; then stem="$1.${arg#--}"; fi ;;
  esac
  prev=$arg
done
[ -f "$d/$stem.out" ] || exit 2
echo "$*" > "$d/$stem.args"
while [ -f "$d/$stem.gate" ]; do sleep 0.02; done
cat "$d/$stem.out"
if [ -f "$d/$stem.hold" ]; then
  sleep 30 &
  echo $! > "$d/pid"
  wait
fi
exit "$(cat "$d/$stem.code" 2>/dev/null || echo 0)"
