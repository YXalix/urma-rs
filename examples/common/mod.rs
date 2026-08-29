//! Utilities shared by the examples: control-plane HTTP helpers, the peer
//! descriptor (`PeerDesc`), the URMA resource bundle (`UrmaRes`), and
//! message/CLI helpers.
//!
//! They live here instead of the library (src/) because they depend on
//! dev-dependencies such as axum/reqwest/serde/tokio, while the library must
//! stay free of third-party dependencies. Each example includes this via
//! `#[path = "common/mod.rs"] mod common;` and uses only the subset it needs
//! (unused items raise no warnings, see the allow below).

#![allow(dead_code)]

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};

use urma_rs::error::{Error, Result};
use urma_rs::{
    query_device, CompletionQueue, Context, Eid, Jetty, JettyId, JettyOpts, Peer, RegisteredBuf,
    SegDesc, TpType, TransMode, Urma, DEFAULT_DEPTH, TOKEN_VALUE,
};

/// Fixed message length (hello/ping-pong message, single lookup record)
pub const MSG_SIZE: usize = 64;
/// Registered memory size for hello/ping-pong: one message + one landing buffer, fits in a page
pub const BUF_SIZE: usize = 4096;
/// Landing buffer offset: [0, MSG_SIZE) holds our own message (the peer may read
/// it at any time in one-sided mode), [SCRATCH_OFF, +MSG_SIZE) is where the peer's
/// message lands; the two must not overlap, or we'd clobber data being read
pub const SCRATCH_OFF: usize = MSG_SIZE;
/// Retry interval for control-plane connect failures
pub const CONNECT_RETRY_S: Duration = Duration::from_secs(1);

/* ============================== HTTP helpers ============================== */

pub fn http_err(e: reqwest::Error) -> Error {
    Error::Io(std::io::Error::other(e))
}

/// Send a request, retrying forever on connect failure (peer not up yet);
/// protocol errors (non-2xx) fail immediately — this is what lets the two
/// sides start in any order.
pub async fn send_retry(
    http: &reqwest::Client,
    build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
) -> Result<reqwest::Response> {
    let mut retry = 0;
    loop {
        match build(http).send().await {
            Ok(r) if r.status().is_success() => return Ok(r),
            Ok(r) => return Err(Error::Invalid(format!("http status {}", r.status()))),
            Err(e) if e.is_connect() => {
                retry += 1;
                eprintln!("[client] connect to peer failed: {e} (retry {retry})");
                tokio::time::sleep(CONNECT_RETRY_S).await;
            }
            Err(e) => return Err(http_err(e)),
        }
    }
}

/// `send_retry` plus JSON response parsing
pub async fn json_retry<T: serde::de::DeserializeOwned>(
    http: &reqwest::Client,
    build: impl Fn(&reqwest::Client) -> reqwest::RequestBuilder,
) -> Result<T> {
    send_retry(http, build)
        .await?
        .json()
        .await
        .map_err(|e| Error::Invalid(format!("bad json: {e}")))
}

/// Control-plane handshake for hello/ping-pong: `GET /info` fetches the peer
/// descriptor, `POST /import` announces ourselves. HTTP is done after that (the
/// data plane and teardown sync use their own mechanisms).
pub async fn exchange_desc(
    http: &reqwest::Client,
    base: &str,
    my: &PeerDesc,
) -> Result<PeerDesc> {
    let peer: PeerDesc = json_retry(http, |c| c.get(format!("{base}/info"))).await?;
    send_retry(http, |c| c.post(format!("{base}/import")).json(my)).await?;
    Ok(peer)
}
/// axum graceful-shutdown signal: finish on the first of done (one round
/// complete) or stop (the early-exit channel used when this process's
/// client fails).
pub async fn graceful_shutdown(done: Arc<Notify>, mut stop_rx: watch::Receiver<bool>) {
    tokio::select! {
        _ = done.notified() => {},
        _ = stop_rx.changed() => {},
    }
}

/* ============================== message & identity ============================== */

/// Actual length of a C-string-style message (zero padding stripped)
pub fn cstr_len(b: &[u8]) -> usize {
    b.iter().position(|&c| c == 0).unwrap_or(b.len())
}

/// Zero-pad text into a fixed MSG_SIZE-byte message
pub fn fill_msg(text: &str) -> Vec<u8> {
    let mut m = vec![0u8; MSG_SIZE];
    let n = text.len().min(MSG_SIZE - 1);
    m[..n].copy_from_slice(&text.as_bytes()[..n]);
    m
}

/// Default identity: hostname, falling back to pid if unreadable
pub fn default_name(name: Option<&str>) -> String {
    name.map(str::to_string).unwrap_or_else(|| {
        let h = std::fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if h.is_empty() { format!("pid-{}", std::process::id()) } else { h }
    })
}

/// Uniform exit-code reporting
pub fn report(r: Result<()>) -> ExitCode {
    match r {
        Ok(()) => {
            println!("[main] demo finished");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("[client] error: {e}");
            ExitCode::FAILURE
        }
    }
}

/* ============================== peer descriptor ============================== */

/// JSON form of the peer's data-plane resources (segment + jetty). After the
/// control-plane exchange, `import_peer` imports from it. The plain fields are
/// kept for logging and the loopback check; the import itself runs on the
/// exported blobs (`urma_get_seg_ctx` / `urma_get_rjetty`), which on bonding
/// carry the per-physical-device info and let the import skip the kernel-side
/// seg/jetty exchange (the urma_perftest bonding path). All empty in tcp-hook
/// mode.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct PeerDesc {
    pub eid: [u8; 16],
    pub uasid: u32,
    pub seg_va: u64,
    pub seg_len: u64,
    pub seg_flag: u32,
    pub seg_token_id: u32,
    pub jetty_eid: [u8; 16],
    pub jetty_uasid: u32,
    pub jetty_id: u32,
    /// `RegisteredSeg::export_seg_ctx` blob (urma_seg_t + bonding ext)
    pub seg_ctx: Vec<u8>,
    /// `Jetty::export_rjetty` blob (urma_rjetty_t + bonding ext)
    pub rjetty: Vec<u8>,
}

impl PeerDesc {
    pub fn of(seg: SegDesc, jetty: JettyId, seg_ctx: Vec<u8>, rjetty: Vec<u8>) -> Self {
        PeerDesc {
            eid: seg.eid.0,
            uasid: seg.uasid,
            seg_va: seg.va,
            seg_len: seg.len,
            seg_flag: seg.attr,
            seg_token_id: seg.token_id,
            jetty_eid: jetty.eid.0,
            jetty_uasid: jetty.uasid,
            jetty_id: jetty.id,
            seg_ctx,
            rjetty,
        }
    }

    pub fn to_pair(&self) -> (SegDesc, JettyId) {
        (
            SegDesc {
                eid: Eid(self.eid),
                uasid: self.uasid,
                va: self.seg_va,
                len: self.seg_len,
                attr: self.seg_flag,
                token_id: self.seg_token_id,
            },
            JettyId {
                eid: Eid(self.jetty_eid),
                uasid: self.jetty_uasid,
                id: self.jetty_id,
            },
        )
    }
}

/* ============================== peer import ============================== */

/// `Peer::import_ctx` with a bounded retry for the kernel-exchange class of
/// failure. The blob import on bonding skips the kernel-side seg/jetty
/// exchange, so errno ENOEXEC should no longer occur; the retry stays as
/// cheap insurance for mixed-version peers still publishing plain
/// descriptors. The remaining failure class is the stale per-process
/// topology snapshot (import_jetty "Failed to find connected port").
pub fn import_peer(ctx: &Context, desc: &PeerDesc) -> Result<Peer> {
    /// ubagg maps any exchange failure to -ENOEXEC (ubagg_seg.c/ubagg_jetty.c)
    const ENOEXEC: i32 = 8;
    /// retry headroom: IMPORT_RETRIES x CONNECT_RETRY_S
    const IMPORT_RETRIES: u32 = 30;
    let mut attempt = 0;
    loop {
        match Peer::import_ctx(ctx, &desc.seg_ctx, &desc.rjetty, TpType::Ctp, TOKEN_VALUE) {
            Ok(p) => return Ok(p),
            Err(Error::Null(_, ENOEXEC)) if attempt < IMPORT_RETRIES => {
                if attempt == 0 {
                    eprintln!(
                        "[import] kernel-side info exchange with the peer failed (errno \
                         {ENOEXEC}); retrying up to {IMPORT_RETRIES}s (with blob \
                         descriptors this points at a stale/empty blob or a mixed-version \
                         peer)"
                    );
                }
                attempt += 1;
                std::thread::sleep(CONNECT_RETRY_S);
            }
            Err(e) => {
                if !matches!(e, Error::Null(_, ENOEXEC)) {
                    eprintln!(
                        "[import] note: if the liburma log shows 'Failed to find connected \
                         port', this process's topology snapshot predates the peer's links \
                         (taken once per process, never refreshed); restarting this process \
                         now that the peer is up resolves it"
                    );
                }
                return Err(e);
            }
        }
    }
}

/* ============================== mode preflight ============================== */

/// Preflight the fixed communication mode all examples run — RM transport,
/// CTP tp type at import, multi-path on bonding devices (see docs/urma.md):
/// query the device capabilities first, so an unsupported mode fails here
/// with the device's supported-mode matrix instead of surfacing as an opaque
/// jetty-creation / import error (or a crash) much later. Never call it in
/// tcp-hook mode (no device exists there).
pub fn check_mode_support(dev: &str) -> Result<()> {
    let cap = query_device(dev)?;
    let multi_path = dev.starts_with("bonding");

    let mut missing = Vec::new();
    if !cap.supports_mode(TransMode::Rm) {
        missing.push(format!("transport mode {}", TransMode::Rm.name()));
    } else {
        if !cap.supports(TransMode::Rm, TpType::Ctp) {
            missing.push(format!(
                "tp type {} for {}{}",
                TpType::Ctp.name(),
                TransMode::Rm.name(),
                if cap.tp_cap(TransMode::Rm).ctp && !cap.ctp_en {
                    " (the mode allows CTP but the device feature ctp_en is off)"
                } else {
                    ""
                }
            ));
        }
        if multi_path && !cap.supports_multi_path(TransMode::Rm) {
            missing.push("multi-path for RM (bonding devices run RM with multi-path)".to_string());
        }
    }
    if !missing.is_empty() {
        return Err(Error::Invalid(format!(
            "device '{dev}' does not support the CTP-RM mode these examples run; \
             missing: {}; device supports: {cap}",
            missing.join(", ")
        )));
    }
    println!("[mode] device {dev} supports: {cap}");
    Ok(())
}

/* ============================== URMA resource bundle ============================== */

/// The full resource set for hello/ping-pong, bundled for convenience. Child
/// resources hold reference-counted handles to their parents, so destruction
/// order is safe regardless of field order. Not Send/Sync: it must stay in the
/// task that created it (examples poll with join! on one thread).
pub struct UrmaRes {
    pub ctx: Context,
    pub cq: CompletionQueue,
    pub jetty: Jetty,
    pub buf: RegisteredBuf,
    /// exported at create time (desc() is infallible); the bonding blobs let
    /// the peer import without the kernel-side exchange
    seg_ctx: Vec<u8>,
    rjetty: Vec<u8>,
    _urma: Urma,
}

impl UrmaRes {
    /// Build the full resource set and place `msg` (MSG_SIZE bytes) in [0, MSG_SIZE):
    /// in hello it waits to be READ by the peer; in ping-pong it is the SEND payload.
    pub fn create(role: &str, dev: &str, msg: &[u8]) -> Result<Self> {
        check_mode_support(dev)?;
        let urma = Urma::init()?;
        let ctx = Context::create(&urma, dev)?;
        println!("[{role}] use device {dev} eid ({})", ctx.eid());
        let cq = CompletionQueue::new(&ctx, DEFAULT_DEPTH)?;
        let jetty = Jetty::new(
            &ctx,
            &cq,
            JettyOpts { multi_path: dev.starts_with("bonding"), ..Default::default() },
        )?;
        let mut buf = RegisteredBuf::new(&ctx, BUF_SIZE, TOKEN_VALUE)?;
        let seg_ctx = buf.export_seg_ctx()?;
        let rjetty = jetty.export_rjetty()?;

        buf[..MSG_SIZE].copy_from_slice(&msg[..MSG_SIZE]);
        println!(
            "[{role}] my message is ready: \"{}\"",
            String::from_utf8_lossy(&msg[..cstr_len(&msg[..MSG_SIZE])])
        );

        Ok(UrmaRes { ctx, cq, jetty, buf, seg_ctx, rjetty, _urma: urma })
    }

    /// Public descriptor: announced to the peer / published to the directory
    pub fn desc(&self) -> PeerDesc {
        PeerDesc::of(
            self.buf.descriptor(),
            self.jetty.id(),
            self.seg_ctx.clone(),
            self.rjetty.clone(),
        )
    }
}

/// tcp-hook mode has no URMA resources; Option unifies the branches
pub fn res_opt(tcp_hook: bool, role: &str, dev: &str, msg: &[u8]) -> Result<Option<UrmaRes>> {
    if tcp_hook {
        Ok(None)
    } else {
        UrmaRes::create(role, dev, msg).map(Some)
    }
}

/// Single-machine loopback guard: when both "nodes" run on one machine they
/// share the device (same EID, uasid 0), and the current urma library crashes
/// with a core dump inside the import path instead of returning an error
/// (observed on bonding_dev_0). Fail cleanly and point at two-node mode.
/// Only meaningful with real descriptors — never call it on hook-mode descs
/// (those are all-zero and would compare equal).
pub fn check_loopback(my: &PeerDesc, peer: &PeerDesc) -> Result<()> {
    if my.eid == peer.eid {
        return Err(Error::Invalid(
            "peer EID equals the local EID (single-machine loopback): this urma \
             library/driver build core-dumps on self-import; run the two nodes \
             on separate machines instead"
                .into(),
        ));
    }
    Ok(())
}
