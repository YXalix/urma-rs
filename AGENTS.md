# AGENTS.md — urma-rs

Safe Rust bindings for the URMA (Unified Remote Memory Access) userspace C API,
plus example demos.

## Layout

- `src/ffi.rs` — hand-written raw bindings, transcribed from the vendored
  headers in `include/`; links system `liburma.so`. No build.rs.
- `src/urma.rs` — safe RAII wrapper layer. `src/error.rs` — Error/Result.
  `src/lib.rs` — module wiring + crate-root re-exports.
- `examples/` — `urma_hello` (one-sided READ), `urma_pingpong` (two-sided
  SEND/RECV), `urma_lookup` (P2P directory), `list_devices` (probe tool);
  shared helpers in `examples/common/mod.rs` (pulled in via `#[path]`).
- `scripts/` — local and real-device test entry points.

## Commands

```bash
cargo build --examples      # lib + examples (linking needs liburma.so)
cargo test                  # 4 ffi layout guard tests (sizes/offsets/flags)
cargo clippy --examples
./scripts/test_hello.sh     # local e2e, tcp-hook mode (no device needed)
./scripts/test_pingpong.sh  # local e2e
./scripts/test_local.sh 3 2 # lookup: master + 3 clients x 2 records
./scripts/test_ub.sh        # real-device tests; SKIPs when no device
```

## Layer rules

- **ffi.rs is a transcription layer**: unused constants/types there
  (`URMA_TOKEN_*`, status codes, etc.) are intentional completeness — do not
  "clean" them. Layout unit tests at the bottom of ffi.rs guard the ABI; if
  headers under `include/` change, re-verify ffi.rs against them and update
  the assertions.
- **The library has zero runtime dependencies** (empty `[dependencies]`) —
  never add one. axum/tokio/reqwest/serde/clap are dev-dependencies for the
  examples' HTTP control plane only.
- **Safe layer ownership**: resources form a tree
  (Urma → Context → CompletionQueue → Jetty; Context → RegisteredSeg / Peer);
  children hold `Rc` handles to parents so drop order is always safe. The
  tree is `!Send`/`!Sync` by design — examples keep resources on one task and
  interleave via `tokio::join!`.
- **Import convention**: use crate-root re-exports (`use urma_rs::{...}`),
  not `urma_rs::urma::...`. If a new public item should be reachable at the
  root, add it to the `pub use` list in `src/lib.rs`.
- **Examples stay self-contained**: the Args/validate/run/combined skeleton is
  deliberately duplicated across urma_hello/urma_pingpong so each demo reads
  standalone. Only genuinely shared helpers go in `examples/common/mod.rs`
  (it carries `#![allow(dead_code)]` for that reason).

## Domain facts

- The token-id (protection table) mechanism is intentionally disabled:
  `token_policy`/`token_id_valid` are all 0; authentication is only the plain
  `token_value` agreed by both peers (`TOKEN_VALUE = 0xACFE`).
- `PageBuf` keeps 4KB alignment as cheap insurance (not required by the plain
  register path); use the `PAGE_SIZE` const, never a magic 4096.

## Gotchas

- **Do not run `cargo fmt` on the repo**: the code is hand-formatted in a
  compact style that default rustfmt reformats everywhere. Match the
  surrounding style and keep newly written lines fmt-clean.
- The examples' control plane is HTTP JSON (Rust-to-Rust), not a stable wire
  protocol.
- tcp-hook mode emulates the data plane over HTTP for logic testing on
  machines without a URMA device; it has no one-sided semantics.
