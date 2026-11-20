#!/bin/sh
# An editor, run as vim with `+line file`, that leaves `ran` beside it and
# returns once `gate` there is gone, as one left open a while would.
d=$(dirname "$0")
touch "$d/ran"
while [ -f "$d/gate" ]; do sleep 0.02; done
