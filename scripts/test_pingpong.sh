#!/bin/bash
# Local check for the urma_pingpong example: run two local processes pointing
# at each other and verify both exit 0 and finish one ping-pong round
# (client receives "pong from <peer>", server's send completes).
# Modes (MODE env var): hook (default, no device) | ub (real URMA, needs DEV=<name>)
set -u
cd "$(dirname "$0")/.."

# clear proxy env vars so 127.0.0.1 requests do not go through an external proxy
unset http_proxy https_proxy all_proxy no_proxy \
      HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY 2>/dev/null || true

MODE=${MODE:-hook}
case "$MODE" in
    hook) MODE_ARGS=(--tcp-hook); VIA="tcp-hook" ;;
    ub)
        if [ -z "${DEV:-}" ]; then
            echo "MODE=ub requires DEV=<device name> (run the list_devices example)" >&2
            exit 1
        fi
        MODE_ARGS=(-d "$DEV"); VIA="URMA SEND/RECV" ;;
    *) echo "MODE must be hook or ub" >&2; exit 1 ;;
esac

BIN=./target/debug/examples/urma_pingpong
PA=${PA:-15059}
PB=${PB:-15060}

cargo build --examples || exit 1

LOGDIR=$(mktemp -d /tmp/urma_rs_pingpong.XXXXXX)
cleanup() { rm -rf "$LOGDIR"; }
trap cleanup EXIT

"$BIN" "${MODE_ARGS[@]}" -i 127.0.0.1 -p "$PB" -P "$PA" -n nodeA >"$LOGDIR/a.log" 2>&1 &
A=$!
"$BIN" "${MODE_ARGS[@]}" -i 127.0.0.1 -p "$PA" -P "$PB" -n nodeB >"$LOGDIR/b.log" 2>&1 &
B=$!

FAIL=0
wait "$A" || FAIL=1
wait "$B" || FAIL=1

# Server-side done evidence: send-completion log in URMA mode, /msg reply log in hook mode
if [ "$MODE" = ub ]; then
    DONE_LINE="pong consumed by peer (send completed), no bye needed"
else
    DONE_LINE="ping via tcp-hook"
fi

for pair in "a:nodeB" "b:nodeA"; do
    log=${pair%%:*}
    peer=${pair##*:}
    line="pong via $VIA: \"pong from $peer\""
    if ! grep -qF "$line" "$LOGDIR/$log.log"; then
        echo "MISSING in $log.log: $line"
        FAIL=1
    fi
    if ! grep -qF "$DONE_LINE" "$LOGDIR/$log.log"; then
        echo "MISSING in $log.log: server done ($DONE_LINE)"
        FAIL=1
    fi
done

if [ "$FAIL" -eq 0 ]; then
    echo "PASS: both nodes finished a ping-pong round via $VIA (no out-of-band bye)"
else
    echo "FAIL: logs kept in $LOGDIR (rerun to regenerate)"
    trap - EXIT
fi
exit $FAIL
