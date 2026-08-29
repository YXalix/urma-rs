# URMA concepts

Background notes on the URMA (Unified Remote Memory Access) userspace API,
limited to what these bindings and examples rely on. The authoritative
references are the vendored headers in `include/` and the umdk source tree:

- `/root/openEuler/umdk/doc/ch/urma/URMA API Guide.ch.md` — full API guide
  (Chinese); §1.2 has the parameter-combination rules
- `/root/openEuler/umdk/src/urma/examples/urma_sample.c` — canonical C sample
  (trans_mode/tp_type selectable via `-m`/`-t`)

## Resource model

```
urma_device_t                     one per NIC / bonding device
 └─ urma_context_t                create_context(dev, eid_index)
     ├─ urma_jfc_t                completion queue
     ├─ urma_jfr_t                recv queue      ─┐ a "jetty" couples a
     ├─ urma_jfs_t                send queue      ─┤ jfs with (shared) jfr
     ├─ urma_jetty_t              endpoint        ─┘
     ├─ urma_seg_t                registered memory (local)
     ├─ urma_target_seg_t         imported remote segment
     └─ urma_target_jetty_t       imported remote jetty
```

Local resources are created; remote ones are **imported** from a descriptor
the peer publishes out of band (EID, uasid, ids, token). Import is what
establishes connectivity — the local jetty itself carries no peer address.

## Transport parameters

Three independent knobs, validated together at create/import time:

- `trans_mode` — URMA software concept, transaction-layer connectivity:
  - `URMA_TM_RM` — reliable message (connectionless, many-to-many)
  - `URMA_TM_RC` — reliable connection (one-to-one, needs `urma_bind_jetty`)
  - `URMA_TM_UM` — unreliable message
- `tp_type` — UB-protocol TP type:
  - `URMA_RTP` — Reliable Transport
  - `URMA_CTP` — Compact Transport
  - `URMA_UTP` — Unreliable Transport
- `order_type` — UB-protocol ordering: `DEF_ORDER` (0, driver picks per
  trans_mode), `OI` (initiator), `OT` (target), `OL` (low layer),
  `NO` (unreliable non-ordering)

Combination rules (API Guide §1.2, abridged to the rows that matter here):

| trans_mode | tp_type | order_type | verdict |
|---|---|---|---|
| RM | RTP / CTP | DEF(0), OI, OL | allowed |
| RM | RTP / CTP | NO | blocked (RM is reliable) |
| RM | UTP | any | blocked (UTP is UM-only) |
| RC | RTP / CTP | DEF(0), OT, OL, OI | allowed (OT chip-dependent) |
| UM | UTP | DEF(0) / NO | allowed |
| UM | RTP, or CTP+anything but OL | — | blocked |

UTP is legal only under UM; RM/RC reject order_type NO.

## tp_type is chosen at import, not at create

`urma_jfs_cfg_t` / `urma_jfr_cfg_t` have **no** tp_type field. The TP type is
selected by the *importer*, in `urma_rjetty_t.tp_type` passed to
`urma_import_jetty` (same for `urma_rjfr_t`). A created jetty is therefore
agnostic; both sides of a connection must simply agree on the tp_type in
their respective imports. See `urma_sample.c` (`sample_import_jetty`) and
`urma_perftest` for reference usage.

## Querying what a device supports

`urma_query_device` returns `urma_device_attr_t`, whose `dev_cap` carries the
communication-mode capabilities as bitmaps:

- `trans_mode` — bit OR of the supported transport modes (RM/RC/UM);
- `rm_tp_cap` / `rc_tp_cap` / `um_tp_cap` — per-mode tp types (RTP/CTP/UTP);
- `rm_order_cap` / `rc_order_cap` — per-mode order types;
- `tp_feature` — per-mode multi-path support;
- `feature.ctp_en` — a device-level CTP gate *in addition* to the per-mode
  cap bits.

The safe layer wraps this as `query_device(name) -> DeviceCap`
(`DeviceCap::supports(mode, tp)` folds in the `ctp_en` gate) and the examples
preflight their fixed CTP-RM choice through `common::check_mode_support`
before creating any resource, so an unsupported mode fails with the device's
supported-mode matrix instead of an opaque create/import error.
`list_devices -- --caps` prints the matrix per device as a probe
(`urma_cli list --caps` prints the same plus a legend trailer):

```
bonding_dev_0
  modes  : RM[tp=CTP] RC[tp=] UM[tp=]
  combos : RM-CTP
  limits : max_jfs_sge 13 max_jfr_sge 4 max_msg_size 65536 page_size_cap 0x0
```

- `modes` walks every advertised transport mode; inside the brackets: `tp=`
  the tp types the mode can use, `order=` its order types (ot/oi/ol/no),
  `multi-path` when the mode can span several physical ports. An empty
  `tp=` (`RC[tp=]` above) means the mode bit is set but no tp type is
  usable, so the mode is effectively unavailable.
- `combos` is the flattened answer — every (mode, tp) pair that passes
  `supports()` — i.e. the valid `--mode`/`--tp` values for the examples.
- `limits` — `max_jfs_sge`/`max_jfr_sge`: max scatter-gather entries per
  send / receive work request; `max_msg_size`: largest single message in
  bytes; `page_size_cap`: page-size bitmap for pinned registration
  (0 = the provider did not report one; the plain register path does not
  need it).

## What this repo uses

All examples run **CTP-RM**:

- `trans_mode = URMA_TM_RM` — reliable message, with the multi-path flag on
  the jfs (`RC + single-path` is the loopback-only combination `urma_sample`
  permits on bonding devices; RM + multi-path is the sanctioned two-node one)
- `tp_type = URMA_CTP` — set in `Peer::import` (`src/urma.rs`), the single
  `urma_import_jetty` call site
- `order_type = 0` (DEF_ORDER) — allowed for RM+CTP per the table above

CTP requires device support: `urma_device_feature.ctp_en` and the
`rm_tp_cap.ctp` capability bit (see the querying section above; the examples
check both up front via `check_mode_support`). On a device without CTP the
examples stop before creating resources; they do not fall back to RTP.
