#!/bin/sh
# A session: starts a listener with the dffy it's given, writing where nobody
# reads, and stays as its parent.
"$1" comments listen --as claude > /dev/null 2>&1 &
wait
