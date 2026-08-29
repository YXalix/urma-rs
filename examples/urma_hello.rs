//! urma_hello —— minimal URMA one-sided READ demo, single-file example.
//!
//! Topology: two nodes, one process each; every process runs both a server task
//! (axum) and a client task, each reading "hello world from <node name>" from the
//! other's memory:
//!
//! ```text
//! node A                                         node B
//! server: exposes "hello world from A"           server: exposes "hello world from B"
//! client ---URMA READ---> B's registered memory  client ---URMA READ---> A's registered memory
//! ```
//!
//! - server is control plane only: `GET /info` publishes the segment/jetty
//!   descriptor (503 until the resources exist), `POST /import` / `POST /bye`
//!   receive peer notices, `POST /abort` tells us the peer's client died; in URMA
//!   mode the data plane needs no CPU — the one-sided READ is done by the NIC
//!   straight from local registered memory.
//! - the memory side never notices being read, so after reading the client
//!   sends `POST /bye` and only then may the peer free its registered memory.
//! - URMA resources are created lazily, only after the peer process has answered
//!   (any HTTP status): the bonding provider snapshots the fabric topology once
//!   per process at context creation and never refreshes it, so a snapshot taken
//!   before the peer node's links were up makes `urma_import_jetty` fail with
//!   "Failed to find connected port". Deferring creation keeps start order free.
//! - the server bounds its wait for the peer's bye (timeout + `/abort`): HTTP is
//!   connectionless, so unlike TCP there is no EOF to notice when the peer
//!   process dies; without a bound the survivor would hang.
//! - `--tcp-hook`: emulates the data plane with a `GET /msg` request-reply for
//!   local logic testing without a URMA device; not one-sided.
//! - `HELLO_RES_DELAY_MS=<n>`: after the peer's server first answers, hold off
//!   creating the URMA resources by n milliseconds before taking the once-per-
//!   process topology snapshot. Experiment knob for the import-time crash seen
//!   on bonding_dev_0 when the snapshot lands while the peer node is still
//!   bringing up its links/resources (observed as SIGSEGV inside liburma's
//!   urma_import_jetty path, milder cousin: "Failed to find connected port").
//!   Delaying is the only app-side lever: both sides gating on the peer's 200
//!   would deadlock (each waits for the other to publish first), and the
//!   snapshot cannot be retaken later in the same process. 0 keeps the
//!   historical create-on-first-reply behavior.
//! - `URMA_LOG_LEVEL`: switch for the liburma-to-stderr log mirror — `off`
//!   keeps the library on its default syslog sink (quiet terminal), `0`-`7`
//!   picks a `URMA_VLOG_LEVEL_*`, unset keeps DEBUG.
//!
//! Usage (run on both nodes at once, each pointing at the other's IP; the client
//! retries forever, so start order doesn't matter):
//!   cargo run --example urma_hello -- -d bonding_dev_0 -i <peer_ip> -n nodeA
//! Local test (no device needed): ./scripts/test_hello.sh

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use tokio::sync::{watch, Notify};

use urma_rs::error::{Error, Result};
use urma_rs::{enable_stderr_log_from_env, Eid};

#[path = "common/mod.rs"]
mod common;

use common::{
    cstr_len, check_loopback, default_name, fill_msg, http_err, import_peer, report, send_retry,
    PeerDesc, UrmaRes, CONNECT_RETRY_S, MSG_SIZE, SCRATCH_OFF,
};

const DEFAULT_PORT: u16 = 13857;
const READ_CTX: u64 = 0x1234;
/// how long the server waits for the peer's bye before giving up (the READ
/// itself completes in milliseconds; anything close to this means the peer
/// died without notice)
const BYE_TIMEOUT: Duration = Duration::from_secs(120);

/// server-shared descriptor slot: None until the lazily created resources
/// publish it (`GET /info` answers 503 meanwhile). Plain data, so unlike the
/// `UrmaRes` itself it can live in axum State.
type DescSlot = Arc<std::sync::RwLock<Option<PeerDesc>>>;

/* ============================== server (axum control plane) ============================== */

/// Server shared state: only Copy-able data such as descriptors/messages. The
/// UrmaRes itself is held by combined (keeping the registered memory alive)
/// and stays out of axum State — it holds raw pointers and is not Send/Sync.
#[derive(Clone)]
struct SrvState {
    desc: DescSlot,
    msg: Vec<u8>,
    /// bye notice from the peer client (read done, we may free resources)
    done: Arc<Notify>,
    /// the peer's client failed before finishing; stop serving and exit
    abort: Arc<Notify>,
}

async fn get_info(State(st): State<SrvState>) -> Response {
    let desc = st.desc.read().unwrap();
    match &*desc {
        Some(d) => (StatusCode::OK, Json(d.clone())).into_response(),
        None => (StatusCode::SERVICE_UNAVAILABLE, "descriptor not ready").into_response(),
    }
}

async fn get_msg(State(st): State<SrvState>) -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/octet-stream")], st.msg.clone())
}

async fn post_import(Json(p): Json<PeerDesc>) -> StatusCode {
    println!(
        "[server] peer ({}, uasid 0x{:x}) imported my segment, serving its READ...",
        Eid(p.eid),
        p.uasid
    );
    StatusCode::NO_CONTENT
}

async fn post_bye(State(st): State<SrvState>) -> StatusCode {
    st.done.notify_one();
    println!("[server] peer finished reading, bye");
    StatusCode::NO_CONTENT
}

async fn post_abort(State(st): State<SrvState>) -> StatusCode {
    st.abort.notify_one();
    println!("[server] peer aborted before finishing, shutting down");
    StatusCode::NO_CONTENT
}

/// why the server stopped serving
#[derive(Clone, Copy)]
enum SrvEnd {
    /// one round complete
    Bye,
    /// the peer's client failed early
    Aborted,
    /// our own client failed; its error is reported on that side
    Stopped,
    /// the peer never sent bye (dead or hung)
    Timeout,
}

/// server: serves the descriptor (once published) and keeps the process's
/// single `res` (owned by combined, created after the peer is up) alive by
/// outliving the client task; bounds the wait for the peer's bye.
async fn server_run(
    desc: DescSlot,
    msg: Vec<u8>,
    port: u16,
    mut stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    let done = Arc::new(Notify::new());
    let abort = Arc::new(Notify::new());
    let state = SrvState { desc, msg, done: Arc::clone(&done), abort: Arc::clone(&abort) };
    let app = Router::new()
        .route("/info", get(get_info))
        .route("/msg", get(get_msg))
        .route("/import", post(post_import))
        .route("/bye", post(post_bye))
        .route("/abort", post(post_abort))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    println!("[server] listening on 0.0.0.0:{port}");

    /* first end condition wins; the result travels through the shared slot
       because with_graceful_shutdown discards the signal future's output */
    let end = Arc::new(std::sync::Mutex::new(None::<SrvEnd>));
    let sig = {
        let end = Arc::clone(&end);
        async move {
            let e = tokio::select! {
                _ = done.notified() => SrvEnd::Bye,
                _ = abort.notified() => SrvEnd::Aborted,
                _ = stop_rx.changed() => SrvEnd::Stopped,
                _ = tokio::time::sleep(BYE_TIMEOUT) => SrvEnd::Timeout,
            };
            *end.lock().unwrap() = Some(e);
        }
    };
    axum::serve(listener, app).with_graceful_shutdown(sig).await?;
    let ended = *end.lock().unwrap();
    match ended {
        Some(SrvEnd::Aborted) => Err(Error::Invalid("peer aborted before finishing the read".into())),
        Some(SrvEnd::Timeout) => Err(Error::Invalid(format!(
            "peer did not finish reading within {BYE_TIMEOUT:?}; it may have died"
        ))),
        _ => Ok(()),
    }
}

/* ============================== client ============================== */

/// Create the URMA resource set on first need and publish the descriptor; a
/// no-op once published. Deliberately called only after the peer process has
/// answered: the bonding provider snapshots the fabric topology once per
/// process (at context creation) and never refreshes it, so snapshotting before
/// the peer node was up leaves `urma_import_jetty` with no connected port. In
/// tcp-hook mode there are no resources and the placeholder descriptor is
/// published instead.
fn ensure_resources(
    res: &mut Option<UrmaRes>,
    desc: &std::sync::RwLock<Option<PeerDesc>>,
    dev: &str,
    msg: &[u8],
    hook: bool,
) -> Result<()> {
    if desc.read().unwrap().is_some() {
        return Ok(());
    }
    if !hook {
        *res = Some(UrmaRes::create("node", dev, msg)?);
    }
    let d = res.as_ref().map(|r| r.desc()).unwrap_or_default();
    *desc.write().unwrap() = Some(d);
    Ok(())
}

async fn client_run(
    http: &reqwest::Client,
    base: &str,
    res: &mut Option<UrmaRes>,
    desc: &DescSlot,
    dev: &str,
    msg: &[u8],
    hook: bool,
) -> Result<()> {
    /* control-plane handshake, ordered around the topo snapshot: wait for the
       peer process to answer (any status), create our resources only then, and
       keep polling until the peer has published its descriptor. Gating on the
       peer's 200 instead would deadlock the symmetric pair, so the snapshot
       can only be deferred by time (HELLO_RES_DELAY_MS), never by events. */
    let res_delay_ms: u64 = std::env::var("HELLO_RES_DELAY_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut first_reply = true;
    let mut retry = 0u32;
    let info = loop {
        match http.get(format!("{base}/info")).send().await {
            Ok(r) => {
                /* snapshot once per process at context creation: hold it until
                   after the peer's settle window if asked (experiment knob) */
                if first_reply {
                    first_reply = false;
                    if res_delay_ms > 0 && !hook {
                        println!(
                            "[client] peer reachable; deferring resource creation \
                             {}ms so the topology snapshot lands late \
                             (HELLO_RES_DELAY_MS)",
                            res_delay_ms
                        );
                        tokio::time::sleep(Duration::from_millis(res_delay_ms)).await;
                    }
                }
                ensure_resources(res, desc, dev, msg, hook)?;
                if r.status().as_u16() == 200 {
                    break r
                        .json()
                        .await
                        .map_err(|e| Error::Invalid(format!("bad json: {e}")))?;
                }
                /* peer up, its descriptor not ready yet */
                println!(
                    "[client] peer answered {}, descriptor not published yet, retrying",
                    r.status()
                );
                tokio::time::sleep(CONNECT_RETRY_S).await;
            }
            Err(e) if e.is_connect() => {
                retry += 1;
                eprintln!("[client] connect to peer failed: {e} (retry {retry})");
                tokio::time::sleep(CONNECT_RETRY_S).await;
            }
            Err(e) => return Err(http_err(e)),
        }
    };
    let my = res.as_ref().map(|r| r.desc()).unwrap_or_default();
    send_retry(http, |c| c.post(format!("{base}/import")).json(&my)).await?;
    if !hook {
        check_loopback(&my, &info)?;
    }

    let msg: Vec<u8> = if hook {
        /* emulated data plane: request-reply */
        let b = send_retry(http, |c| c.get(format!("{base}/msg"))).await?;
        b.bytes().await.map_err(http_err)?.to_vec()
    } else {
        let r = res.as_ref().expect("urma mode requires resources");
        let (seg, _jetty) = info.to_pair();
        println!(
            "[client] importing peer ({}, uasid 0x{:x}): seg va 0x{:x} len {} attr 0x{:x} token_id {}, jetty uasid 0x{:x} id {} (my jetty id {}, my token_id {})",
            Eid(info.eid),
            info.uasid,
            info.seg_va,
            info.seg_len,
            info.seg_flag,
            info.seg_token_id,
            info.jetty_uasid,
            info.jetty_id,
            my.jetty_id,
            my.seg_token_id
        );
        let p = import_peer(&r.ctx, &info)?;
        println!(
            "[client] peer ({}, uasid 0x{:x}) segment: va 0x{:x} len {}",
            seg.eid, seg.uasid, seg.va, seg.len
        );
        let sge = r.buf.sge(SCRATCH_OFF, MSG_SIZE as u32)?;
        r.jetty.post_read(
            &p,
            seg.va, /* READ semantics: src is a remote address */
            &[sge],
            READ_CTX,
        )?;
        r.cq.wait_read(READ_CTX)?;
        r.buf[SCRATCH_OFF..SCRATCH_OFF + MSG_SIZE].to_vec()
    };

    let end = cstr_len(&msg);
    println!(
        "[client] read {} bytes via {}: \"{}\"",
        msg.len(),
        if hook { "tcp-hook" } else { "URMA READ" },
        String::from_utf8_lossy(&msg[..end])
    );

    /* tell the peer the read is done so it can free resources safely */
    send_retry(http, |c| c.post(format!("{base}/bye"))).await?;
    Ok(())
}

/* ============================== entry ============================== */

/// Minimal URMA one-sided READ demo: 2 nodes, server + client on each node.
#[derive(clap::Parser)]
#[command(
    name = "urma_hello",
    version,
    about = "Minimal URMA one-sided READ demo: 2 nodes, server + client on each node.",
    after_help = "\
Usage (run on both nodes, each pointing at the other's IP):
  urma_hello -d <device> -i <peer_ip> [-p port] [-n name]   run server + client
Local test (no device): add --tcp-hook, e.g. `./scripts/test_hello.sh`

The client retries connecting forever until the peer is up, so the two
nodes can be started in any order. Ctrl+C exits.

Environment:
  HELLO_RES_DELAY_MS=<n>  defer the once-per-process topology snapshot by n ms
                          after the peer first answers (import-crash experiment;
                          0 = create resources on the peer's first reply)
  URMA_LOG_LEVEL=off|0-7  liburma log mirror to stderr; off = default syslog
                          sink only, unset = DEBUG)"
)]
struct Args {
    /// URMA device, e.g. udma2 / bonding_dev_0 (see examples/list_devices)
    #[arg(short, long = "dev")]
    dev_name: Option<String>,

    /// peer node IP (the node whose server we read from)
    #[arg(short = 'i', long = "peer-ip")]
    peer_ip: Option<String>,

    /// identity in the message (default: hostname)
    #[arg(short, long)]
    name: Option<String>,

    /// emulate the data plane over HTTP (local logic test only)
    #[arg(short = 'T', long = "tcp-hook")]
    tcp_hook: bool,

    /// TCP port to connect on the peer
    #[arg(short, long, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// local TCP listen port (default: same as --port)
    #[arg(short = 'P', long = "listen-port")]
    listen_port: Option<u16>,
}

impl Args {
    /// Cross-field constraints
    fn validate(&self) -> std::result::Result<(), String> {
        if !self.tcp_hook && self.dev_name.is_none() {
            return Err("URMA mode requires -d <device> (or use --tcp-hook)".into());
        }
        if self.peer_ip.is_none() {
            return Err("requires -i <peer_ip>".into());
        }
        Ok(())
    }
}

async fn run(args: &Args, dev: &str, name: &str) -> Result<()> {
    if !args.tcp_hook {
        /* provider/driver errors otherwise go to syslog only; URMA_LOG_LEVEL
           is the switch (off / 0-7, default DEBUG) */
        enable_stderr_log_from_env()?;
    }
    let listen_port = args.listen_port.unwrap_or(args.port);
    let msg = fill_msg(&format!("hello world from {name}"));
    combined(args, dev, &msg, listen_port).await
}

/// One UrmaRes per process, shared by the server and client tasks: urma_init
/// succeeds only once per process (the second call returns URMA_EEXIST), so two
/// role-local resource sets would silently kill whichever task initializes
/// second. The 4K buffer already reserves [0, MSG_SIZE) for our message and
/// [SCRATCH_OFF, +MSG_SIZE) as the peer-message landing zone, so one
/// registration serves both directions. The set is created lazily by the client
/// task once the peer answers (topo-snapshot ordering, see ensure_resources)
/// and lives in this frame, so the registered memory outlives both tasks and is
/// freed only after the server saw the peer's bye (or gave up). join! polls both
/// tasks on one thread, so the raw-pointer UrmaRes never needs Send.
async fn combined(args: &Args, dev: &str, msg: &[u8], listen_port: u16) -> Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);

    let mut res: Option<UrmaRes> = None;
    let desc: DescSlot = Arc::new(std::sync::RwLock::new(None));
    if args.tcp_hook {
        /* no resources to wait for; /info is ready from the start */
        *desc.write().unwrap() = Some(PeerDesc::default());
    }

    let srv = async { server_run(Arc::clone(&desc), msg.to_vec(), listen_port, stop_rx).await };

    let cli = async {
        let http = reqwest::Client::new();
        let base = format!("http://{}:{}", args.peer_ip.as_deref().unwrap(), args.port);
        let r = client_run(&http, &base, &mut res, &desc, dev, msg, args.tcp_hook).await;
        if r.is_err() {
            let _ = stop_tx.send(true); /* tell the server task to exit so join! doesn't hang */
            /* best effort: stop the peer's server from waiting for our bye */
            let _ = http.post(format!("{base}/abort")).send().await;
        }
        r
    };

    let (cli, srv) = tokio::join!(cli, srv);
    cli.and(srv)
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if let Err(m) = args.validate() {
        eprintln!("error: {m}\n\nFor more information, try '--help'.");
        return ExitCode::from(2);
    }
    let name = default_name(args.name.as_deref());
    let dev = args.dev_name.as_deref().unwrap_or("");
    report(run(&args, dev, &name).await)
}
