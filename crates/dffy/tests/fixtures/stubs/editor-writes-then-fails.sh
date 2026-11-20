#!/bin/sh
# An editor, run as vim with `+line file`, that adds ` typed` to the file
# and then fails, as one quit with an error after a write would.
printf ' typed\n' >> "$2"
exit 1
