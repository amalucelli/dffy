#!/bin/sh
# The `gh` on review.rs's PATH: it replays the fixtures a test put in
# $STUB/gh and logs each request. Each event records how many lines the
# plugin had printed by then, which orders the plugin's output against its
# requests without sleeps. GraphQL requests go to `requests` and `events`,
# REST ones to `rest`.
case "$1" in
  --version) echo 'gh version 2.0.0 (stub)'; exit 0 ;;
  auth)
    printf '%s\n' "$*" >> "$STUB/auth"
    if [ -f "$STUB/gh/logged-out" ]; then
      echo 'You are not logged into any GitHub hosts. To log in, run: gh auth login' >&2; exit 1
    fi
    printf 'github.com\n  ✓ Logged in to github.com account octocat (keyring)\n'; exit 0 ;;
esac
body=$(cat)
if [ -f "$STUB/gh/logged-out" ]; then
  echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4
fi
if [ -f "$STUB/gh/bad-token" ]; then echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1; fi
if [ "$2" != graphql ]; then
  # REST, named by the path's last segment. A `.hold` makes the answer wait
  # for the plugin's first line; the log says how many lines it had then.
  op=$(printf '%s' "$2" | sed 's/?.*//; s|.*/||')
  n=$(($(cat "$STUB/rest" 2>/dev/null | grep -c "/$op?") + 1))
  if [ -f "$STUB/gh/$op.hold" ]; then
    i=0
    while [ "$(wc -l < "$STUB/out")" -lt 1 ] && [ $i -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
  fi
  printf '%s after %s\n' "$*" "$(wc -l < "$STUB/out" | tr -d ' ')" >> "$STUB/rest"
  for f in "$STUB/gh/$op.$n" "$STUB/gh/$op"; do
    if [ -f "$f.json" ]; then cat "$f.json"; exit 0; fi
    if [ -f "$f.down" ]; then echo 'error connecting to api.github.com' >&2; exit 1; fi
  done
  echo "gh stub: no fixture for $op call $n" >&2
  exit 1
fi
op=$(printf '%s' "$body" | sed -n 's/.*"operationName":"\([A-Za-z]*\)".*/\1/p')
printf '%s\n' "$body" >> "$STUB/requests"
printf 'gh %s after %s\n' "$op" "$(wc -l < "$STUB/out" | tr -d ' ')" >> "$STUB/events"
n=$(grep -c "\"operationName\":\"$op\"" "$STUB/requests")
# Comment ops another dffy applies to the comments file at $STUB_COMMENTS
# while this request is in flight.
if [ -f "$STUB/gh/$op.$n.apply.json" ]; then
  dffy --comments "$STUB_COMMENTS" comments apply < "$STUB/gh/$op.$n.apply.json" > /dev/null
fi
for f in "$STUB/gh/$op.$n" "$STUB/gh/$op"; do
  if [ -f "$f.json" ]; then cat "$f.json"; exit 0; fi
  if [ -f "$f.error.json" ]; then cat "$f.error.json"; echo "gh: GraphQL error" >&2; exit 1; fi
  if [ -f "$f.down" ]; then echo 'error connecting to api.github.com' >&2; exit 1; fi
done
echo "gh stub: no fixture for $op call $n" >&2
exit 1
