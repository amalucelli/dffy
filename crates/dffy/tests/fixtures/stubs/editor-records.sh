#!/bin/sh
# An editor that writes to `args` beside it whether it had the terminal,
# tty or notty, and its arguments.
if [ -t 0 ]; then tty='tty'; else tty='notty'; fi
echo "$tty $*" > "$(dirname "$0")/args"
