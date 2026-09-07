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
| `urma_bench` | one-sided READ/WRITE latency-bandwidth benchmark: URMA vs TCP, swept over message sizes | URMA: `post_read`/`post_write` + busy-polled completion (server CPU uninvolved); TCP has no one-sided op, so a read is emulated as a 4-byte length request + N-byte response round trip (TCP_NODELAY on both ends); per size one verify pass + warmup + timed iters, reported as min/p50/avg/p99/max (`--csv` for machine-readable rows); `read-urma --depth N>1` switches to a pipelined bandwidth measurement (both transfer ends rotating their windows, avg MiB/s + Mops, `--duration` for seconds-long windows); `write-urma` runs the same sweep for the WRITE direction (verify = serialized WRITE + READ-back); same pure-std style as `urma_cli` |

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
- Default ports: hello 13857, lookup 13858, pingpong 13859, urma_bench
  13860 (its TCP reference plane; the URMA side has no port — the
  descriptor travels by copy-paste like `urma_cli`'s).
- The Args/validate/run/combined skeleton is deliberately duplicated
  across hello/pingpong so each demo reads standalone.

## Run locally (no URMA device)

```bash
cargo build --examples
./scripts/test_hello.sh      # urma_hello, two local processes
./scripts/test_pingpong.sh   # urma_pingpong
./scripts/test_local.sh 3 2  # urma_lookup: master + 3 clients x 2 records
cargo run --example urma_bench -- serve-tcp &  # TCP loopback table (real
                                              # sockets, no device involved)
cargo run --example urma_bench -- read-tcp --addr 127.0.0.1
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
#     modes  : RM[tp=RTP,CTP order=oi multi-path] RC[tp=] UM[tp=]
#     combos : RM-RTP RM-CTP
#     limits : max_jfs_sge 13 max_jfr_sge 4 max_msg_size 65536 max_read_size 1048576
#              max_write_size 1048576 page_size_cap 0x0
#   (legend: modes = transport modes RM/RC/UM with their usable tp types; ...)

# on both nodes, each pointing at the other's IP:
cargo run --example urma_hello    -- -d bonding_dev_0 -i <peer_ip> -n nodeA
cargo run --example urma_pingpong -- -d bonding_dev_0 -i <peer_ip> -n nodeA

# lookup: the master runs anywhere (no device needed), one client per node:
cargo run --example urma_lookup -- --master --clients 2
cargo run --example urma_lookup -- -d bonding_dev_0 -m <master_ip> -n nodeA
```

How to read the `--caps` block (`urma_cli list --caps` prints the same, plus
a legend trailer): `modes` walks every transport mode the device advertises —
RM reliable message, RC reliable connection, UM unreliable message — with the
tp types the mode can use (`tp=RTP,CTP`,..), its order types (`order=oi`,..)
and `multi-path` when it can span several physical ports. An empty `tp=`
(e.g. `RC[tp=]`) means the mode bit is set but no tp type is usable, so the
mode is effectively unavailable. `combos` is the flattened answer: every
(mode, tp) pair you may actually use (CTP additionally requires the
device-level `ctp_en` gate) — the valid `--mode`/`--tp` values for the demos.
`limits` caps a workload: `max_jfs_sge`/`max_jfr_sge` scatter-gather entries
per send/recv, `max_msg_size` largest two-sided message in bytes, `max_read_size` /
`max_write_size` largest one one-sided READ / WRITE in bytes (0 = not
reported by the provider), `page_size_cap` page-size bitmap for pinned
registration. The concepts behind these knobs are in `docs/urma.md`.

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

## Latency / bandwidth benchmark: urma_bench

`urma_bench` compares URMA one-sided READ latency against TCP over the same
size sweep. TCP cannot do one-sided I/O, so its "read" is the standard
request/response emulation (4-byte length request → N-byte response, one
RTT per op, TCP_NODELAY forced); URMA measures the real one-sided READ
(post + busy-polled completion, the server CPU never touched). Both sides
share the sweep/verify/warmup discipline so the columns are comparable,
and sizes above the device's READ ceiling (`max_read_size`, falling
back to `max_msg_size` when unreported) or above the registered segment
are skipped with a note. Run it between the same pair of machines:

```bash
# TCP reference over the IP fabric:
nodeA$ cargo run --example urma_bench -- serve-tcp
nodeB$ cargo run --example urma_bench -- read-tcp --addr <ipA>

# URMA over the UB fabric (descriptor by copy-paste, like urma_cli);
# serve sizes its segment to the sweep's maximum, so the sides cannot drift:
nodeA$ cargo run --example urma_bench -- serve-urma -d bonding_dev_0 --sizes 8..16m
nodeB$ cargo run --example urma_bench -- read-urma -d bonding_dev_0 '<the [desc] hex line>' \
        --sizes 8..16m

# URMA READ bandwidth: same serve side, reader pipelines --depth READs;
# give serve a --buf-len above the sweep max so the reader can rotate the
# remote window too, and use --duration for seconds-long stable windows:
nodeA$ cargo run --example urma_bench -- serve-urma -d bonding_dev_0 --sizes 4k..1m --buf-len 64m
nodeB$ cargo run --example urma_bench -- read-urma -d bonding_dev_0 '<the [desc] hex line>' \
        --sizes 4k..1m --depth 32 --duration 10

# URMA WRITE bandwidth (perftest write_bw, same knobs): the asymmetry probe
# next to the READ number. WRITEs overwrite the serve's pattern, so restart
# serve-urma before a read-urma verify against the same segment:
nodeA$ cargo run --example urma_bench -- serve-urma -d bonding_dev_0 --sizes 4k..1m --buf-len 64m
nodeB$ cargo run --example urma_bench -- write-urma -d bonding_dev_0 '<the [desc] hex line>' \
        --sizes 4k..1m --depth 512 --post-list 32 --jetties 4 --duration 10
```

`--sizes` (comma list `8,64,1k` or doubling range `8..16m`, k/m/g
suffixes allowed), `--iters`, `--warmup`, `--csv` control the sweep —
the serve subcommands accept `--sizes` too and size their buffer to its
maximum, so no manual `--buf-len` bookkeeping is needed.

`read-urma --depth N` (default 1) turns the URMA side into a bandwidth
measurement in urma_perftest read_bw's style: up to N (≤ 64, the
jetty/CQ depth) READs in flight, post-one/reap-one. Both transfer ends
rotate through their disjoint windows per op — the reader registers a
size×depth landing buffer (capped at 1 GiB, with a note when the cap
bites; give serve-urma a `--buf-len` above the sweep max to widen the
remote rotation, since hammering one address range saturates that memory
region rather than the link). Each size reports average BW over the whole
first-post→last-completion window plus Mops — deliberately no peak
column: in a full pipeline every per-op post→completion window contains
queueing time, so a "fastest op" is pipeline noise; the `--csv` schema
changes accordingly (`ops,depth,bw_avg_mib,mops`). The verify pass stays
serialized. `--duration S` runs each size to a seconds-long deadline
instead of `--iters` ops (warmup becomes a fixed 1s): with iteration
counts a big-size row's window shrinks to tens of milliseconds and one
scheduler hiccup dominates the average. TCP stays at depth 1 by
construction — a request/response pair cannot pipeline, which is itself
the semantic gap.
