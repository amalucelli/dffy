#!/bin/sh
# A plugin for dffy's tests, installed as dffy-review-test or dffy-agent: a
# JSON-RPC server on stdin and stdout, one message a line, answering from
# files beside it that the test writes. Each start adds a line to
# `started`: its working directory, tty or notty (whether it had a
# terminal) and what dffy set in DFFY. Each `initialize` adds tty or notty
# to `described` and its params to `initialized`; every other message adds
# `<method> <params>` to `calls`.
#
# A message is answered from files named by a stem, its method with `/`
# as `_` and without `$` (`$/cancelRequest` is `_cancelRequest`):
#   <stem>.out    printed, with `@ID@` as the request's id, or for a
#                 notification its params' id; a request without one is a
#                 method the stub doesn't know
#   <stem>.gate   the answer waits while it exists, the stub reading on
#   <stem>.hold   after the answer, a child whose pid goes in `pid` keeps
#                 running
#   <stem>.crash  the stub exits with status 3 instead of answering
#   <stem>.die    the stub exits with status 3 once it printed the answer,
#                 as a plugin that crashes partway
# `shutdown` answers null unless told otherwise; `exit` exits.
d=$(dirname "$0")
if ( : < /dev/tty ) 2>/dev/null; then tty='tty'; else tty='notty'; fi
echo "$(pwd -P) $tty dffy=$DFFY" >> "$d/started"

answer() {
  while [ -f "$d/$1.gate" ]; do sleep 0.02; done
  if [ -f "$d/$1.out" ]; then
    sed "s/@ID@/$2/g" "$d/$1.out"
  elif [ -n "$3" ]; then
    printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"no %s"}}\n' "$2" "$1"
  fi
  if [ -f "$d/$1.hold" ]; then
    sleep 30 &
    echo $! > "$d/pid"
    wait
  fi
}

while IFS= read -r line; do
  method=$(printf '%s\n' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  id=$(printf '%s\n' "$line" | sed -n 's/^{"jsonrpc":"2.0","id":\([0-9]*\),.*/\1/p')
  params=$(printf '%s\n' "$line" | sed -n 's/.*"params":\(.*\)}$/\1/p')
  stem=$(printf '%s' "$method" | tr '/' '_' | tr -d '$')
  if [ "$method" = initialize ]; then
    echo "$tty" >> "$d/described"
    printf '%s\n' "$params" >> "$d/initialized"
  else
    printf '%s %s\n' "$method" "$params" >> "$d/calls"
  fi
  [ "$method" = exit ] && exit 0
  if [ "$method" = shutdown ] && [ ! -f "$d/shutdown.out" ]; then
    printf '{"jsonrpc":"2.0","id":%s,"result":null}\n' "$id"
    continue
  fi
  [ -f "$d/$stem.crash" ] && exit 3
  request=$id
  if [ -z "$id" ]; then
    id=$(printf '%s\n' "$params" | sed -n 's/.*"id":\([0-9]*\).*/\1/p')
  fi
  if [ -f "$d/$stem.gate" ] || [ -f "$d/$stem.hold" ]; then
    answer "$stem" "$id" "$request" &
  else
    answer "$stem" "$id" "$request"
  fi
  [ -f "$d/$stem.die" ] && exit 3
done
