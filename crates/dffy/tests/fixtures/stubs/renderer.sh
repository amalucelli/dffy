#!/bin/sh
# A renderer for dffy's tests, installed as dffy-render-<behavior>: the
# name it's installed under picks what it does. Each run adds a line to
# `runs` beside it; answers come from `answer.json` beside it.
d=$(dirname "$0")
echo run >> "$d/runs"
case "${0##*/dffy-render-}" in
  # Keeps the request in `request`.
  answers) cat > "$d/request"; cat "$d/answer.json" ;;
  unread) cat "$d/answer.json" ;;
  crash) exit 3 ;;
  garbage) echo nope ;;
  says) printf '{"version":1,"error":"no tables"}'; exit 1 ;;
  broken) cat > /dev/null; echo nope ;;
  slow) cat > /dev/null; sleep 5 ;;
  # Would answer nothing and log `end` after 3s.
  late) cat > /dev/null; sleep 3; echo end >> "$d/runs" ;;
  # Never reads the request; its child's pid goes in `pid`.
  hangs)
    sleep 30 &
    echo $! > "$d/pid"
    wait
    ;;
  # Answers, leaving a child running whose pid goes in `pid`.
  helper)
    sleep 30 > /dev/null 2>&1 &
    echo $! > "$d/pid"
    cat > /dev/null
    cat "$d/answer.json"
    ;;
  # Its own pid and its child's go in `pids` at once.
  forks)
    sleep 300 &
    echo "$$ $!" > "$d/pids.tmp"
    mv "$d/pids.tmp" "$d/pids"
    cat > /dev/null
    wait
    ;;
esac
