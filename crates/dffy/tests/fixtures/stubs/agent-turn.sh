#!/bin/sh
# A coding agent a Fix with agent handoff starts in the working tree, taking
# its turn: it writes the checkpoints there were when it started to
# $STUB/refs, one ref a line, and the prompt it was given to $STUB/prompt,
# then edits src/lib.rs and exits.
git for-each-ref --format='%(refname)' refs/worktree/dffy/checkpoints/ > "$STUB/refs"
printf '%s' "$1" > "$STUB/prompt"
echo "line from the agent" >> src/lib.rs
