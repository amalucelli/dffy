#!/bin/sh
# The ssh git runs for review.rs's `git@github.com:` remotes: it drops the
# host, logs the command git asks for (`git-upload-pack 'acme/widgets.git'`)
# to $STUB/ssh.log, and runs it in $STUB_REMOTES, where the bare
# repositories are.
while [ $# -gt 1 ]; do shift; done
printf '%s\n' "$1" >> "$STUB/ssh.log"
cd "$STUB_REMOTES" && exec /bin/sh -c "$1"
