# urma-rs — Rust bindings for URMA

Safe Rust wrapper for the URMA (Unified Remote Memory Access) userspace C API,
plus demos in `examples/`. The library has zero third-party dependencies;
axum/tokio/reqwest/serde/clap are dev-dependencies used only by the examples'
HTTP control plane.

## Layout

- `src/` — the library: `ffi.rs` (hand-written raw bindings transcribed from
  the vendored headers in `include/`, ABI guarded by unit tests, links system
  `liburma.so`), `urma.rs` (safe RAII wrapper), `error.rs`
- `examples/` — demos: `urma_hello` (one-sided READ), `urma_pingpong`
  (two-sided SEND/RECV), `urma_lookup` (P2P directory), `list_devices`
  (probe tool); see `examples/README.md`
- `scripts/` — test entry points
- `docs/urma.md` — URMA concepts (resource model, trans_mode/tp_type/
  order_type rules, why the examples are CTP-RM)

## Build

Requires `liburma.so` on the system.

```bash
cargo build --examples   # library + examples
cargo test               # ffi layout guard tests
```

## Test

Local logic checks (tcp-hook mode emulates the data plane over HTTP, no URMA
device needed):

```bash
./scripts/test_hello.sh      # urma_hello
./scripts/test_pingpong.sh   # urma_pingpong
./scripts/test_local.sh 3 2  # urma_lookup: master + 3 clients x 2 records
```

Real-device runs need **two** UB machines (UB has no single-machine
loopback). Configure the node pair as an IP list and run over ssh:

```bash
UB_NODES="192.168.1.11 192.168.1.12" ./scripts/test_ub.sh
```

The script builds the examples, deploys them to both nodes, and runs
hello + pingpong + lookup across the pair (SKIPs when no node list is
configured or a node has no device). See `examples/README.md` for running
the demos by hand.

## Logging

The URMA libraries log to **syslog** (facility `user`, tagged `[URMA]`);
this crate carries no logging code of its own. Errors already appear at the
default level; for full detail set `URMA_LOG_LEVEL=debug` and
`UVS_LOG_LEVEL=debug` before starting the process, then read
`journalctl -f | grep '\[URMA\]'`. See `docs/logging.md` for the full
mechanism (env vars, levels, rsyslog paths, kernel-side `dmesg`).
