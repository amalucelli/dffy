#!/bin/sh
# The `amp` on agent.rs's PATH. dffy-agent only finds it, to name it as an
# agent a thread can be handed to; it never runs it, so this fails if run.
echo "amp stub: dffy-agent ran it with $*" >&2
exit 2
