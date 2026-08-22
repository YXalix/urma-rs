#!/bin/bash
# Local check for the urma_lookup example: master + N clients on this machine;
# verify all processes exit 0 and every client fetched all foreign records
# with the expected content.
# Usage: ./scripts/test_local.sh [client count (default 3, max 8)] [records per client (default 2)]
# Modes (MODE env var): hook (default, no device) | ub (real URMA, needs DEV=<name>)
set -u
cd "$(dirname "$0")/.."

# clear proxy env vars so 127.0.0.1 requests do not go through an external proxy
unset http_proxy https_proxy all_proxy no_proxy \
      HTTP_PROXY HTTPS_PROXY ALL_PROXY NO_PROXY 2>/dev/null || true

MODE=${MODE:-hook}
case "$MODE" in
    hook) CLIENT_MODE_ARGS=(--tcp-hook) ;;
    ub)
        if [ -z "${DEV:-}" ]; then
            echo "MODE=ub requires DEV=<device name> (run the list_devices example)" >&2
            exit 1
        fi
        CLIENT_MODE_ARGS=(-d "$DEV") ;;
    *) echo "MODE must be hook or ub" >&2; exit 1 ;;
esac

BIN=./target/debug/examples/urma_lookup
N=${1:-3}
R=${2:-2}
PORT=${PORT:-14958}
NAMES=(nodeA nodeB nodeC nodeD nodeE nodeF nodeG nodeH)

if [ "$N" -gt "${#NAMES[@]}" ] || [ "$N" -lt 1 ]; then
    echo "client count must be in [1, ${#NAMES[@]}]" >&2
    exit 1
fi
cargo build --examples || exit 1

LOGDIR=$(mktemp -d /tmp/urma_rs_test.XXXXXX)
cleanup() { rm -rf "$LOGDIR"; }
trap cleanup EXIT

"$BIN" --master --clients "$N" --port "$PORT" >"$LOGDIR/master.log" 2>&1 &
MPID=$!
sleep 0.3

PIDS=()
for ((i = 0; i < N; i++)); do
    "$BIN" "${CLIENT_MODE_ARGS[@]}" -m 127.0.0.1 --port "$PORT" --name "${NAMES[$i]}" --records "$R" \
        >"$LOGDIR/${NAMES[$i]}.log" 2>&1 &
    PIDS+=($!)
done

FAIL=0
for pid in "${PIDS[@]}"; do
    wait "$pid" || FAIL=1
done
wait "$MPID" || FAIL=1

# Parse the range actually assigned by master from each client log
# (accept order is arbitrary, do not assume it)
declare -A MY_FIRST=()
for ((i = 0; i < N; i++)); do
    me=${NAMES[$i]}
    f=$(sed -n 's/.*holding records \[\([0-9]*\),.*/\1/p' "$LOGDIR/$me.log" | head -1)
    if [ -z "$f" ]; then
        echo "MISSING: cannot parse record range of $me"
        FAIL=1
    else
        MY_FIRST[$me]=$f
    fi
done

# rid -> owner mapping
declare -A OWNER=()
for me in "${!MY_FIRST[@]}"; do
    for ((rid = MY_FIRST[$me]; rid < MY_FIRST[$me] + R; rid++)); do
        OWNER[$rid]=$me
    done
done

# Check the fetched lines in each client log one by one (skip own records)
total=$((N * R))
for ((rid = 0; rid < total; rid++)); do
    owner=${OWNER[$rid]}
    [ -n "$owner" ] || { echo "MISSING: no owner for record $rid"; FAIL=1; continue; }
    for ((i = 0; i < N; i++)); do
        me=${NAMES[$i]}
        f=${MY_FIRST[$me]:-}
        [ -n "$f" ] || continue
        if [ "$rid" -ge "$f" ] && [ "$rid" -lt "$((f + R))" ]; then
            continue
        fi
        line="fetched record $rid from $owner: \"record $rid from $owner\""
        if ! grep -qF "$line" "$LOGDIR/$me.log"; then
            echo "MISSING in $me log: $line"
            FAIL=1
        fi
    done
done

if [ "$FAIL" -eq 0 ]; then
    echo "PASS: $N clients x $R records, all foreign records fetched with correct content (${MODE:-hook})"
else
    echo "FAIL: logs kept in $LOGDIR (rerun to regenerate)"
    trap - EXIT
fi
exit $FAIL
