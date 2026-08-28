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

## What this repo uses

All examples run **CTP-RM**:

- `trans_mode = URMA_TM_RM` — reliable message, with the multi-path flag on
  the jfs (`RC + single-path` is the loopback-only combination `urma_sample`
  permits on bonding devices; RM + multi-path is the sanctioned two-node one)
- `tp_type = URMA_CTP` — set in `Peer::import` (`src/urma.rs`), the single
  `urma_import_jetty` call site
- `order_type = 0` (DEF_ORDER) — allowed for RM+CTP per the table above

CTP requires device support: `urma_device_feature.ctp_en` and the
`rm_tp_cap.ctp` capability bit (queryable via `urma_query_device`). On a
device without CTP the import fails; the examples do not fall back to RTP.
