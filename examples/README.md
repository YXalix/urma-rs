# examples — URMA demos

Each `.rs` file here is a standalone example, runnable as
`cargo run --example <name>`; genuinely shared helpers live in
`common/mod.rs` (pulled in via `#[path]`, not a crate — it carries
`#![allow(dead_code)]` because each example uses only a subset). Every
demo starts with a doc comment explaining its protocol flow, and all of
them support `--tcp-hook` to emulate the data plane over HTTP for local
logic testing without a URMA device.

| example | what it shows | URMA semantics |
| --- | --- | --- |
| `urma_hello` | two nodes read "hello world" from each other | one-sided READ; out-of-band `/bye` teardown |
| `urma_pingpong` | ping-pong echo between two nodes | two-sided SEND/RECV; in-band teardown via CQ completions |
| `urma_lookup` | record directory: HTTP-only master assigns id ranges, clients fetch records from each other P2P | one-sided READ; data plane bypasses the master |
| `list_devices` | prints URMA device names, one per line | probe tool (`urma_get_device_list`), exit 0 iff a device exists; `--caps` appends each device's supported communication-mode matrix (`urma_query_device`) |
| `urma_cli` | minimal pure-URMA CLI, the single-file usage example of the whole API | `list [--caps]` device probe, `serve`/`read` one-sided READ where the descriptor travels by manual copy-paste (no HTTP control plane at all); `--mode`/`--tp` select any advertised communication combination, both sides must agree |

## Conventions

- **Control plane is HTTP JSON** (axum server + reqwest client in the same
  binary): it does only what URMA cannot — publishing and exchanging the
  segment+jetty descriptor (`PeerDesc`). It is a Rust-to-Rust demo
  protocol, not a stable wire format. `urma_cli` is the deliberate
  exception: it has no control plane at all (no tokio/serde either, and it
  does not include `common/mod.rs`) — `serve` prints the descriptor as one
  hand-packed hex line that you paste as `read`'s positional argument on
  the other node, which also sidesteps the bonding topology-snapshot
  ordering by construction (the reading side only starts once `serve` is
  up).
- **Clients retry forever on connect failure**, so peers can start in any
  order; Ctrl+C exits. Non-2xx replies fail immediately.
- **Server + client in one process** (hello/pingpong): `tokio::join!` polls
  both tasks on one thread, because the resource tree (`UrmaRes`) holds
  raw pointers and is `!Send`/`!Sync`.
- **Memory layout** (hello/pingpong): `[0, MSG_SIZE)` is the published
  message the peer may read at any time; `[SCRATCH_OFF, +MSG_SIZE)` is the
  landing buffer for incoming data. The two must never overlap.
- Default ports: hello 13857, lookup 13858, pingpong 13859.
- The Args/validate/run/combined skeleton is deliberately duplicated
  across hello/pingpong so each demo reads standalone.

## Run locally (no URMA device)

```bash
cargo build --examples
./scripts/test_hello.sh      # urma_hello, two local processes
./scripts/test_pingpong.sh   # urma_pingpong
./scripts/test_local.sh 3 2  # urma_lookup: master + 3 clients x 2 records
```

These use `--tcp-hook`: the data plane is emulated by HTTP request-reply
(the peer's CPU moves the data), so it has no one-sided semantics.

## Run on real nodes

Real-device runs need **two** UB machines (UB has no single-machine
loopback). The scripts/test_ub.sh entry runs all three demos across a
configured node pair over ssh:

```bash
UB_NODES="192.168.1.11 192.168.1.12" ./scripts/test_ub.sh
```

Or run the demos by hand:

```bash
cargo run --example list_devices   # pick a device name, e.g. bonding_dev_0
cargo run --example list_devices -- --caps   # + per-device supported-mode matrix
#   bonding_dev_0
#     modes  : RM[tp=RTP,CTP order=oi multi-path] ...
#     combos : RM-RTP RM-CTP ...   limits: max_jfs_sge ... max_msg_size ...

# on both nodes, each pointing at the other's IP:
cargo run --example urma_hello    -- -d bonding_dev_0 -i <peer_ip> -n nodeA
cargo run --example urma_pingpong -- -d bonding_dev_0 -i <peer_ip> -n nodeA

# lookup: the master runs anywhere (no device needed), one client per node:
cargo run --example urma_lookup -- --master --clients 2
cargo run --example urma_lookup -- -d bonding_dev_0 -m <master_ip> -n nodeA
```

Before creating any resource, every real-device run also preflights the
fixed CTP-RM mode against these capabilities
(`common::check_mode_support`): on an unsupported device it fails
immediately with the supported-mode matrix instead of an opaque
jetty-creation / import error.

The minimal path — `urma_cli`, no HTTP at all, the descriptor travels by
copy-paste (defaults are CTP-RM like the other demos; `--mode`/`--tp` run
any combination the device advertises, both sides must pass the same
values):

```bash
# terminal on nodeA: registers memory, prints the descriptor, holds it
cargo run --example urma_cli -- serve -d bonding_dev_0
# terminal on nodeB: paste the [desc] hex line as the positional argument
cargo run --example urma_cli -- read -d bonding_dev_0 '<the [desc] hex line>'
```

Every example documents its full option set in `--help`;
common flags: `-d/--dev` device, `-i/--peer-ip` / `-m/--master-ip` peer,
`-T/--tcp-hook` emulated data plane, `-p/-P` connect/listen ports,
`-n/--name` identity in messages.
