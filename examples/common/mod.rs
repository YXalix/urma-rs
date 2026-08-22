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
    CompletionQueue, Context, Eid, Jetty, JettyId, JettyOpts, RegisteredBuf, SegDesc, Urma,
    DEFAULT_DEPTH, TOKEN_VALUE,
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
    my: PeerDesc,
) -> Result<PeerDesc> {
    let peer: PeerDesc = json_retry(http, |c| c.get(format!("{base}/info"))).await?;
    send_retry(http, |c| c.post(format!("{base}/import")).json(&my)).await?;
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
/// control-plane exchange, `to_pair()` rebuilds the arguments for
/// `Peer::import`. All zeros in tcp-hook mode.
#[derive(Serialize, Deserialize, Default, Clone, Copy)]
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
}

impl PeerDesc {
    pub fn of(seg: SegDesc, jetty: JettyId) -> Self {
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
        }
    }

    pub fn to_pair(self) -> (SegDesc, JettyId) {
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
    _urma: Urma,
}

impl UrmaRes {
    /// Build the full resource set and place `msg` (MSG_SIZE bytes) in [0, MSG_SIZE):
    /// in hello it waits to be READ by the peer; in ping-pong it is the SEND payload.
    pub fn create(role: &str, dev: &str, msg: &[u8]) -> Result<Self> {
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

        buf[..MSG_SIZE].copy_from_slice(&msg[..MSG_SIZE]);
        println!(
            "[{role}] my message is ready: \"{}\"",
            String::from_utf8_lossy(&msg[..cstr_len(&msg[..MSG_SIZE])])
        );

        Ok(UrmaRes { ctx, cq, jetty, buf, _urma: urma })
    }

    /// Public descriptor: announced to the peer / published to the directory
    pub fn desc(&self) -> PeerDesc {
        PeerDesc::of(self.buf.descriptor(), self.jetty.id())
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
