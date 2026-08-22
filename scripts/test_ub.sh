#!/bin/bash
# Unified entry for real-device (UB) testcases: runs only if a URMA/UB device
# is present (probed via examples/list_devices), otherwise SKIP (exit 0).
#   1) urma_hello:    MODE=ub DEV=<dev> ./scripts/test_hello.sh
#   2) urma_lookup:   MODE=ub DEV=<dev> ./scripts/test_local.sh
#   3) urma_pingpong: MODE=ub DEV=<dev> ./scripts/test_pingpong.sh
# Usage: ./scripts/test_ub.sh [client count (default 2)] [records per client (default 2)]
#   DEV=<name>  pick the device (default: first in the device list)
# Note: single-node loopback needs device/driver loopback support; if
# import/READ fails, use the two-node mode instead.
set -u
cd "$(dirname "$0")/.."

N=${1:-2}
R=${2:-2}

cargo build --examples || exit 1

# Probe: one device name per line on stdout; empty + non-zero exit = no device
DEVS=$(./target/debug/examples/list_devices 2>/dev/null) || true
if [ -z "$DEVS" ]; then
    echo "SKIP: no URMA device on this machine (urma_get_device_list empty); UB testcases not run"
    exit 0
fi

DEV=${DEV:-$(echo "$DEVS" | head -1)}
echo "UB device(s) found: $(echo "$DEVS" | tr '\n' ' ')(using $DEV)"

MODE=ub DEV="$DEV" ./scripts/test_hello.sh || exit 1
MODE=ub DEV="$DEV" ./scripts/test_local.sh "$N" "$R" || exit 1
MODE=ub DEV="$DEV" ./scripts/test_pingpong.sh || exit 1

echo "PASS: UB testcases (urma_hello + urma_lookup + urma_pingpong) on device $DEV"
