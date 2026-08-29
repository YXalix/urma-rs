# AGENTS.md — urma-rs

Safe Rust bindings for the URMA (Unified Remote Memory Access) userspace C API,
plus example demos.

## Layout

- `src/ffi.rs` — hand-written raw bindings, transcribed from the vendored
  headers in `include/`; links system `liburma.so`. No build.rs.
- `src/urma.rs` — safe RAII wrapper layer. `src/error.rs` — Error/Result.
  `src/lib.rs` — module wiring + crate-root re-exports.
- `examples/` — `urma_hello` (one-sided READ), `urma_pingpong` (two-sided
  SEND/RECV), `urma_lookup` (P2P directory), `list_devices` (probe tool,
  `--caps` prints each device's supported-mode matrix), `urma_cli` (minimal
  pure-URMA CLI, the single-file usage example of the whole API: `list
  [--caps]` probe plus `serve`/`read` manual copy-paste READ with
  `--mode`/`--tp` communication-mode selection — no HTTP control plane, no
  tokio/serde, and deliberately no `common/mod.rs` include); shared helpers
  in `examples/common/mod.rs` (pulled in via `#[path]`).
- `scripts/` — local and real-device test entry points.
- `docs/urma.md` — URMA background concepts (resource model, transport
  parameter rules, CTP-RM rationale).

## Commands

```bash
cargo build --examples      # lib + examples (linking needs liburma.so)
cargo test                  # 7 guard tests (5 ffi ABI layout + Urma::init
                            # idempotence + device-cap mode logic)
cargo test --example urma_cli  # +1 wire-descriptor hex round-trip (example
                            # targets are compiled but not run by plain
                            # `cargo test`)
cargo clippy --examples
./scripts/test_hello.sh     # local e2e, tcp-hook mode (no device needed)
./scripts/test_pingpong.sh  # local e2e
./scripts/test_local.sh 3 2 # lookup: master + 3 clients x 2 records
./scripts/test_ub.sh        # two-node UB e2e over ssh; needs
                            # UB_NODES="ipA ipB" (or scripts/ub_nodes.txt),
                            # SKIPs when unset (UB has no loopback)
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

- The demos default to **CTP-RM**: `trans_mode = URMA_TM_RM` (reliable
  message, multi-path) + `tp_type = URMA_CTP` (Compact Transport; UB
  protocol TP types are RTP/CTP/UTP), via `JettyOpts::trans_mode`
  (default `TransMode::Rm`) at create and the `tp: TpType` argument at
  import. The layer is otherwise mode-parameterized — `urma_cli` exposes
  `--mode rm|rc|um` / `--tp rtp|ctp|utp` to run any combination the device
  advertises (both sides must pass the same values). `tp_type` is NOT a
  create-time parameter — `urma_jfs_cfg_t`/`urma_jfr_cfg_t` have no such
  field; it is chosen by the importer in `urma_rjetty_t.tp_type` at
  `urma_import_jetty` (`Peer::import`/`Peer::import_ctx` in `src/urma.rs`).
  RM + CTP + default order_type (0) is an
  allowed combination per umdk's URMA API Guide §1.2 (RM+CTP+NO is blocked,
  RM is reliable); CTP additionally requires device support
  (`urma_device_feature.ctp_en` / `rm_tp_cap.ctp`). See `docs/urma.md` for
  the full background; references:
  `/root/openEuler/umdk/doc/ch/urma/URMA API Guide.ch.md`,
  `/root/openEuler/umdk/src/urma/examples/urma_sample.c` (`-t 1` = CTP).
- Device capabilities are queryable up front: `urma_query_device` is bound
  in ffi.rs (cap structs + layout guards) and wrapped as
  `query_device(name) -> DeviceCap` (`supports(mode, tp)` folds in the
  `ctp_en` gate; `Display` renders the supported-mode matrix). All
  real-device example runs preflight their mode (CTP-RM + RM multi-path on
  bonding) via `common::check_mode_support` before creating any resource —
  `urma_cli` does the same for its selected `--mode`/`--tp` with an in-file
  parameterized copy — so an unsupported mode fails with the matrix instead
  of an opaque create/import error; `list_devices --caps` and
  `urma_cli list --caps` print it as a probe.
- The token-id (protection table) mechanism is intentionally disabled:
  `token_policy`/`token_id_valid` are all 0; authentication is only the plain
  `token_value` agreed by both peers (`TOKEN_VALUE = 0xACFE`). The driver still
  allocates a token id at register (a kernel bitmap id on bonding devices: 0
  only for the first-ever registration), and it is the KEY of the import
  exchange: the importer's kernel sends it to the remote, which resolves the
  segment by it (`ubagg_connect.c handle_seg_req`), so `descriptor()` must
  ship `seg.token_id` as-is (publishing a hardcoded 0 makes import fail — or
  resolve a wrong seg — whenever the peer's allocation isn't 0; conversely, a
  0 in the log is legitimate when it is the peer's first-ever registration,
  not a sign of this bug). Unlike
  token_id, `attr`'s has_user_info bit (ext data we never ship) must be masked
  off before publishing.
- `urma_init` is once-per-process in the C library (second call returns
  `URMA_EEXIST`); `Urma::init` caches the guard per thread so several resource
  sets per process work, and `urma_uninit` runs once, at thread teardown.
  `urma_hello` still uses a single shared `UrmaRes` per process (halves device
  resources); `urma_pingpong` keeps per-role sets (echo needs `&mut` buffer
  access, and one jetty would mix both roles' recv buffers).
- Single-machine loopback (both "nodes" on one device: same EID, uasid 0)
  core-dumps inside liburma's import path instead of returning an error
  (observed on bonding_dev_0). The examples guard it via
  `common::check_loopback` and fail with a clean message; real UB-mode e2e
  needs two machines. Note `urma_sample` only allows loopback on bonding
  devices with RC + single-path; our examples are CTP-RM + multi-path, which
  is the sanctioned two-node combination.
- The bonding provider (liburma_ubagg) snapshots the fabric topology once per
  process — at the first `urma_create_context` (`get_topo_info_from_ko`) — and
  never refreshes it. `urma_import_jetty` picks paths from that snapshot
  (`bondp_rebuild_connected_by_topo` overwrites the kernel matrix), while
  `urma_import_seg` uses the kernel's live matrix, so a process started before
  the peer node's links were up fails import_jetty with "Failed to find
  connected port" (NULL, errno 115) even though the seg import succeeded.
  Restarting the process once the peer is up takes a fresh snapshot and fixes
  it. Hence `urma_hello` creates its `UrmaRes` only after the peer answers
  (`ensure_resources`; `GET /info` returns 503 until then) and bounds the
  server's bye wait (`BYE_TIMEOUT` + `POST /abort`) so a dead peer cannot hang
  the survivor — HTTP, unlike TCP, gives no EOF signal.
  The snapshot can only be deferred by time, never by events (both sides
  gating on the peer's 200 would deadlock): `HELLO_RES_DELAY_MS=<n>` holds
  resource creation back n ms after the peer first answers, as an experiment
  knob for the import-time SIGSEGV.
- A different import failure class is the kernel-side seg/jetty info exchange
  between the two bonding devices (`ubagg_connect_xchg_seg` /
  `ubagg_connect_xchg_jetty`): any failure inside it (peer's agg EID not in
  the kernel's global topo map, comm msg undeliverable, remote token-id
  lookup miss, 30s session timeout) is mapped to `-ENOEXEC`, surfacing as
  `urma_tlv_ioctl ... errno=8, cmd=6` (cmd 6 = URMA_CMD_IMPORT_SEG) and
  `Error::Null("urma_import_seg", 8)`. This path uses the kernel's LIVE topo
  map (fed asynchronously by the fabric management stack via the set-topo
  ioctl), NOT the process's snapshot, so restarting the process does not
  help — diagnose via `dmesg | grep -i ubagg` on BOTH nodes. Observed root
  cause on bonding_dev_0: the kernel ubmad component's own jetty imports
  fail with `UDMA: tp mode is not supported, tp type: 2` (UTP unsupported
  by the physical devices), the management comm channel never comes up, and
  every exchange session times out (`ubagg_session_timeout` in dmesg).
  The robust fix is to BYPASS the exchange: publish the
  `urma_get_seg_ctx` / `urma_get_rjetty` blobs (they append the
  per-physical-device info as has_user_info ext) and import via
  `Peer::import_ctx` — the provider then resolves psegs/pjettys locally
  from its topo snapshot (this is exactly urma_perftest's bonding-duplex
  path, which is why perftest works while plain-descriptor imports fail).
  `urma_get_rjetty` requires a shared jfr (our `Jetty::new` always sets
  SHARE_JFR). All HTTP examples route imports through `common::import_peer`,
  which uses the blob path; `urma_cli` calls `Peer::import_ctx` directly on
  the hand-packed hex descriptor for the same reason; the plain-field
  `PeerDesc` members remain for logging and `check_loopback` only.
- This crate has NO logging layer: the URMA libraries log to syslog
  (facility `user`, `[URMA]` tag) on their own — liburma core honors
  `urma_register_log_func` callbacks, but the tpsa provider (libuvs) always
  writes to syslog directly, so the system log is the only complete source
  and we do not wrap a partial mirror. Levels: `URMA_LOG_LEVEL` /
  `UVS_LOG_LEVEL` env vars, STRING values (`fatal`/`error`/`warning`/`info`/
  `debug`, default `info`), read once in the .so constructor. Full details:
  `docs/logging.md`. NULL-returning FFI calls capture errno into
  `Error::Null(what, errno)` at the failure site.
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
