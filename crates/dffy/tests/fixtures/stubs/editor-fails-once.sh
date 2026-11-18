#!/bin/sh
# An editor, run as vim with `+line file`, that fails the first time,
# leaving `ran` beside it, and then adds ` in vim` to the file.
ran="$(dirname "$0")/ran"
[ -e "$ran" ] || { touch "$ran"; exit 1; }
printf ' in vim\n' >> "$2"
