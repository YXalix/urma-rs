# urma-rs — Rust bindings for URMA

Safe Rust wrapper for the URMA (Unified Remote Memory Access) userspace C API,
plus demos in `examples/` (all support a `--tcp-hook` local test mode) and a
device enumeration tool (`list_devices`). The library has zero third-party
dependencies; axum/tokio/reqwest/serde/clap are dev-dependencies used only by
the examples' HTTP control plane.

## Layout

```
src/                    the library (raw bindings + safe wrapper)
├── ffi.rs              hand-written raw bindings, transcribed from the vendored
│                       headers in include/; layouts guarded by unit tests;
│                       links system liburma.so
├── error.rs            Error/Result
├── urma.rs             safe wrapper (RAII resources + page-aligned buffer)
└── lib.rs
include/                vendored URMA C headers (urma_api/types/opcode.h)
examples/
├── common/mod.rs       shared helpers (PeerDesc, UrmaRes, HTTP retry, etc.)
├── urma_hello.rs       one-sided READ demo (two nodes read each other)
├── urma_pingpong.rs    two-sided SEND/RECV demo (in-band shutdown)
├── urma_lookup.rs      P2P demo (axum master + clients read each other's records)
├── list_devices.rs     URMA device enumeration tool
├── README.md           demo guide (conventions, local + real-device runs)
scripts/
├── test_hello.sh       local check for urma_hello (two processes)
├── test_pingpong.sh    local check for urma_pingpong
├── test_local.sh       local check for urma_lookup (master + N clients)
└── test_ub.sh          real-device test entry (SKIPs when no device found)
```

## Build

Requires liburma.so on the system. The FFI bindings are hand-written.

```bash
cargo build --examples       # library + examples
cargo test                   # ffi layout guard tests
```

## Run

Local checks without a URMA device (tcp-hook mode):

```bash
./scripts/test_hello.sh         # urma_hello
./scripts/test_pingpong.sh      # urma_pingpong
./scripts/test_local.sh 3 2     # urma_lookup: master + 3 clients x 2 records
```

Real device (probes via list_devices, SKIPs when no device is present):

```bash
./scripts/test_ub.sh            # hello + lookup + pingpong
```

Direct example runs (see each example's `--help` for all options):

```bash
cargo run --example list_devices
cargo run --example urma_hello -- -d bonding_dev_0 -i <peer_ip> -n nodeA
cargo run --example urma_pingpong -- -d bonding_dev_0 -i <peer_ip> -n nodeA
cargo run --example urma_lookup -- --master --clients 2
cargo run --example urma_lookup -- -d bonding_dev_0 -m <master_ip> -n nodeA
```
