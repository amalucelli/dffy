#!/bin/sh
# The `gh` on review.rs's PATH: it replays the fixtures a test put in
# $STUB/gh and logs each request. Each event records how many lines the
# plugin had printed by then, past its answer to `initialize` and but what
# it said to write, which orders the plugin's output against its requests
# without sleeps. GraphQL requests go to `requests` and `events`, REST ones
# to `rest`, and every `gh api` names its host first, which goes to
# `hostnames`. A $STUB/gh/slow file makes `--version` and `auth` hang for
# the seconds it holds, as the process it becomes.
printed() {
  n=$(grep -cv '"comments/apply"' "$STUB/out")
  echo $((n > 0 ? n - 1 : 0))
}
case "$1" in
  --version|auth) [ -f "$STUB/gh/slow" ] && exec sleep "$(cat "$STUB/gh/slow")" ;;
esac
case "$1" in
  --version) echo 'gh version 2.0.0 (stub)'; exit 0 ;;
  auth)
    # `auth status --hostname <host>`: logged in to github.com and to the
    # hosts listed in $STUB/gh/hosts.
    printf '%s\n' "$*" >> "$STUB/auth"
    if [ -f "$STUB/gh/logged-out" ]; then
      echo 'You are not logged into any GitHub hosts. To log in, run: gh auth login' >&2; exit 1
    fi
    if [ "$4" = github.com ] || grep -qx "$4" "$STUB/gh/hosts" 2>/dev/null; then
      printf '%s\n  ✓ Logged in to %s account octocat (keyring)\n' "$4" "$4"; exit 0
    fi
    echo "You are not logged into any accounts on $4" >&2; exit 1 ;;
esac
if [ "$1" != api ] || [ "$2" != --hostname ] || [ -z "$3" ]; then
  echo "gh stub: a call without --hostname: $*" >&2; exit 1
fi
all="$*"
printf '%s\n' "$3" >> "$STUB/hostnames"
shift 3
body=$(cat)
if [ -f "$STUB/gh/logged-out" ]; then
  echo 'To get started with GitHub CLI, please run:  gh auth login' >&2; exit 4
fi
if [ -f "$STUB/gh/bad-token" ]; then echo 'gh: Bad credentials (HTTP 401)' >&2; exit 1; fi
if [ "$1" != graphql ]; then
  # REST, named by the path's last segment, a PR itself `pull`. A `.hold`
  # makes the answer wait for the plugin's first line; the log says how
  # many lines it had then. An `.http` answer is a whole response, as
  # `gh api -i` prints it, and fails as gh does past a 2xx.
  path=${1%%\?*}
  case "$path" in
    */pulls/*/*) op=${path##*/} ;;
    */pulls/*) op=pull ;;
    *) op=${path##*/} ;;
  esac
  printf '%s\n' "$op" >> "$STUB/rest-ops"
  n=$(grep -cx "$op" "$STUB/rest-ops")
  if [ -f "$STUB/gh/$op.hold" ]; then
    i=0
    while [ "$(printed)" -lt 1 ] && [ $i -lt 30 ]; do sleep 0.1; i=$((i + 1)); done
  fi
  printf '%s after %s\n' "$all" "$(printed)" >> "$STUB/rest"
  for f in "$STUB/gh/$op.$n" "$STUB/gh/$op"; do
    if [ -f "$f.json" ]; then cat "$f.json"; exit 0; fi
    if [ -f "$f.http" ]; then
      cat "$f.http"
      code=$(sed -n '1s/^HTTP\/[0-9.]* \([0-9]*\).*/\1/p' "$f.http")
      case "$code" in 2??) exit 0 ;; esac
      echo "gh: HTTP $code" >&2; exit 1
    fi
    if [ -f "$f.down" ]; then echo 'error connecting to api.github.com' >&2; exit 1; fi
  done
  echo "gh stub: no fixture for $op call $n" >&2
  exit 1
fi
op=$(printf '%s' "$body" | sed -n 's/.*"operationName":"\([A-Za-z]*\)".*/\1/p')
printf '%s\n' "$body" >> "$STUB/requests"
printf 'gh %s after %s\n' "$op" "$(printed)" >> "$STUB/events"
n=$(grep -c "\"operationName\":\"$op\"" "$STUB/requests")
# The answer waits while a `.gate` for it exists.
while [ -f "$STUB/gh/$op.$n.gate" ]; do sleep 0.02; done
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
