#!/bin/bash
# Local check for the urma_pingpong example: run two local processes pointing
# at each other and verify both exit 0 and finish one ping-pong round
# (client receives "pong from <peer>", server's send completes).
# tcp-hook mode (no device needed); real-device runs are two-machine only,
# see test_ub.sh.
set -u
cd "$(dirname "$0")/.."

# clear proxy env vars so 127.0.0.1 requests do not go through an external proxy
unset http_proxy https_proxy all_proxy no_proxy \
      HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY 2>/dev/null || true

BIN=./target/debug/examples/urma_pingpong
PA=${PA:-15059}
PB=${PB:-15060}

cargo build --examples || exit 1

LOGDIR=$(mktemp -d /tmp/urma_rs_pingpong.XXXXXX)
echo "node logs: $LOGDIR (a.log = nodeA, b.log = nodeB)"
cleanup() { rm -rf "$LOGDIR"; }
trap cleanup EXIT

# TMO: per-node timeout in seconds (default 60); a node killed by timeout or
# exiting nonzero gets its log tail printed below
timeout -k 5 "${TMO:-60}" "$BIN" --tcp-hook -i 127.0.0.1 -p "$PB" -P "$PA" -n nodeA \
    >"$LOGDIR/a.log" 2>&1 &
A=$!
timeout -k 5 "${TMO:-60}" "$BIN" --tcp-hook -i 127.0.0.1 -p "$PA" -P "$PB" -n nodeB \
    >"$LOGDIR/b.log" 2>&1 &
B=$!

FAIL=0
wait "$A" || FAIL=1
wait "$B" || FAIL=1

if [ "$FAIL" -ne 0 ]; then
    tail -n 10 "$LOGDIR/a.log" "$LOGDIR/b.log"
fi

for pair in "a:nodeB" "b:nodeA"; do
    log=${pair%%:*}
    peer=${pair##*:}
    line="pong via tcp-hook: \"pong from $peer\""
    if ! grep -qF "$line" "$LOGDIR/$log.log"; then
        echo "MISSING in $log.log: $line"
        FAIL=1
    fi
    if ! grep -qF "ping via tcp-hook" "$LOGDIR/$log.log"; then
        echo "MISSING in $log.log: server done (ping via tcp-hook)"
        FAIL=1
    fi
done

if [ "$FAIL" -eq 0 ]; then
    echo "PASS: both nodes finished a ping-pong round via tcp-hook (no out-of-band bye)"
else
    echo "FAIL: logs kept in $LOGDIR (rerun to regenerate)"
    trap - EXIT
fi
exit $FAIL
