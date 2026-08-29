//! urma_pingpong —— minimal URMA two-sided SEND/RECV demo (ping-pong echo), single-file example.
//!
//! Unlike urma_hello's one-sided READ, no out-of-band teardown is needed:
//! both sides get a completion on their own CQ — "data consumed by the peer"
//! is itself the notification.
//!
//! Topology: two nodes, one process each; every process runs both a server
//! (echo side) and a client (initiator) task:
//!
//! ```text
//! node A                                           node B
//! client --SEND "ping from A"--> B's JFR           server: post_recv waits for ping
//! client: post_recv waits for pong <--SEND "pong from B"-- server: done after send completes
//! ```
//!
//! - HTTP control plane only does what URMA cannot: exchange peer_info
//!   descriptors (`GET /info` / `POST /import`), then unused;
//! - client finishes when recv completes (pong landed); server finishes when
//!   send completes (pong consumed by the peer JFR) — no /bye anywhere;
//! - `--tcp-hook`: emulates the data plane with a `POST /msg` request-response
//!   for local logic testing without a URMA device (peer CPU moves the data,
//!   so it lacks two-sided semantics).
//!
//! Usage (run on both nodes, each pointing at the other's IP; client retries
//! forever, start order doesn't matter):
//!   cargo run --example urma_pingpong -- -d bonding_dev_0 -i <peer_ip> -n nodeA
//! Local test (no device needed): ./scripts/test_pingpong.sh

use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use tokio::sync::{watch, Notify};

use urma_rs::error::{Error, Result};
use urma_rs::{Completion, CompletionQueue, Eid, POLL_INTERVAL, POLL_RETRIES};

#[path = "common/mod.rs"]
mod common;

use common::{
    cstr_len, check_loopback, default_name, exchange_desc, fill_msg, graceful_shutdown, http_err,
    import_peer, report, res_opt, send_retry, PeerDesc, UrmaRes, MSG_SIZE, SCRATCH_OFF,
};

const DEFAULT_PORT: u16 = 13859;
/* completion user_ctx for two-sided ops: unique per client/server, send/recv */
const CLI_SEND_CTX: u64 = 0xA001;
const CLI_RECV_CTX: u64 = 0xA002;
const SRV_RECV_CTX: u64 = 0xB001;
const SRV_SEND_CTX: u64 = 0xB002;

/// Polls asynchronously until a successful completion with the given user_ctx
/// arrives; other successful completions are consumed and discarded, failed
/// ones return an error. Yields via await (unlike the library's blocking
/// `wait_read`) so it can interleave with the axum serve under join!.
async fn wait_cr(cq: &CompletionQueue, expected: u64) -> Result<Completion> {
    for _ in 0..POLL_RETRIES {
        if let Some(cr) = cq.poll()? {
            if !cr.is_success() {
                return Err(Error::BadCompletion { status: cr.status, user_ctx: cr.user_ctx });
            }
            if cr.user_ctx == expected {
                return Ok(cr);
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    Err(Error::PollTimeout { user_ctx: expected })
}

/* ============================== server (axum control plane + echo loop) ============================== */

/// server shared state: only Copy or plain data like descriptors/messages.
/// UrmaRes itself is held by server_run (keeps registered memory alive) and
/// never enters axum State — it holds raw pointers, so it is not Send/Sync.
#[derive(Clone)]
struct SrvState {
    info: PeerDesc,
    /// descriptor announced by the peer (client) via /import, used to import
    /// when echoing the SEND
    imported: Arc<Mutex<Option<PeerDesc>>>,
    /// tcp-hook data plane reply body (URMA-mode data plane doesn't use HTTP)
    pong: Arc<Vec<u8>>,
    /// one echo round done: set by the /msg handler in hook mode, or by the
    /// echo round after send completes in URMA mode
    done: Arc<Notify>,
}

async fn get_info(State(st): State<SrvState>) -> Json<PeerDesc> {
    Json(st.info)
}

async fn post_import(State(st): State<SrvState>, Json(p): Json<PeerDesc>) -> StatusCode {
    println!(
        "[server] peer ({}, uasid 0x{:x}) imported, waiting for its ping",
        Eid(p.eid),
        p.uasid
    );
    *st.imported.lock().unwrap() = Some(p);
    StatusCode::NO_CONTENT
}

/// tcp-hook data plane: request body = ping, response body = pong
async fn post_msg(State(st): State<SrvState>, body: Bytes) -> impl IntoResponse {
    let end = cstr_len(&body[..body.len().min(MSG_SIZE)]);
    println!(
        "[server] ping via tcp-hook: \"{}\"",
        String::from_utf8_lossy(&body[..end])
    );
    st.done.notify_one();
    ([(header::CONTENT_TYPE, "application/octet-stream")], st.pong.to_vec())
}

/// URMA echo round: post receive buffer → wait for ping (recv completion) →
/// import peer jetty → send pong → wait for send completion. Send completion
/// = pong consumed by the peer JFR, so local resources can be torn down
/// safely afterwards.
async fn echo_round(res: &UrmaRes, imported: &Arc<Mutex<Option<PeerDesc>>>) -> Result<()> {
    let sge = res.buf.sge(SCRATCH_OFF, MSG_SIZE as u32)?;
    res.jetty.post_recv(&[sge], SRV_RECV_CTX)?;
    let cr = wait_cr(&res.cq, SRV_RECV_CTX).await?;
    let n = (cr.completion_len as usize).min(MSG_SIZE);
    let ping = &res.buf[SCRATCH_OFF..SCRATCH_OFF + n];
    println!(
        "[server] ping via URMA RECV: \"{}\"",
        String::from_utf8_lossy(&ping[..cstr_len(ping)])
    );

    /* peer info must be ready: client only sends ping after the /import reply (204) */
    let info = imported
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| Error::Invalid("ping arrived before peer info".into()))?;
    let (seg, _jetty) = info.to_pair();
    let peer = import_peer(&res.ctx, &info)?;
    println!("[server] pong to peer ({}, uasid 0x{:x})", seg.eid, seg.uasid);

    let pong = res.buf.sge(0, MSG_SIZE as u32)?;
    res.jetty.post_send(&peer, &[pong], SRV_SEND_CTX)?;
    wait_cr(&res.cq, SRV_SEND_CTX).await?;
    println!("[server] pong consumed by peer (send completed), no bye needed");
    Ok(())
}

/// server: runs the axum control plane (/info /import, plus /msg in hook
/// mode) concurrently with the URMA echo round; `done` triggers a graceful
/// HTTP shutdown after one round.
/// `stop_rx` lets the process exit early if the local client fails.
async fn server_run(
    res: Option<UrmaRes>,
    pong: Vec<u8>,
    port: u16,
    stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    let info = res.as_ref().map(|r| r.desc()).unwrap_or_default();
    let done = Arc::new(Notify::new());
    let imported = Arc::new(Mutex::new(None));
    let state = SrvState {
        info,
        imported: Arc::clone(&imported),
        pong: Arc::new(pong),
        done: Arc::clone(&done),
    };
    let app = Router::new()
        .route("/info", get(get_info))
        .route("/import", post(post_import))
        .route("/msg", post(post_msg))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    println!("[server] listening on 0.0.0.0:{port}");

    /* URMA echo round runs concurrently with axum; in hook mode the data plane is handled in the /msg handler */
    let echo = async {
        match res {
            Some(res) => {
                let r = echo_round(&res, &imported).await;
                /* one round is the whole demo: release the HTTP server below (in
                 * hook mode the /msg handler notifies done). Notified on error
                 * too, or a failed round would hang the join. */
                done.notify_one();
                r
            }
            None => Ok(()),
        }
    };
    let serve = axum::serve(listener, app)
        .with_graceful_shutdown(graceful_shutdown(Arc::clone(&done), stop_rx));
    let (srv, echo) = tokio::join!(serve, echo);
    srv?;
    echo
}

/* ============================== client ============================== */

async fn client_run(
    http: &reqwest::Client,
    base: &str,
    res: Option<&mut UrmaRes>,
    hook: bool,
    ping: Vec<u8>,
) -> Result<()> {
    /* control-plane handshake: fetch peer descriptor, announce ours; HTTP is
     * done after this — data plane and teardown sync all go through the URMA
     * completion queue */
    let my = res.as_ref().map(|r| r.desc()).unwrap_or_default();
    let info = exchange_desc(http, base, &my).await?;
    if !hook {
        check_loopback(&my, &info)?;
    }

    let msg: Vec<u8> = if hook {
        /* emulated data plane: request-response */
        let b = send_retry(http, |c| c.post(format!("{base}/msg")).body(ping.clone())).await?;
        b.bytes().await.map_err(http_err)?.to_vec()
    } else {
        let r = res.expect("urma mode requires resources");
        let (seg, _jetty) = info.to_pair();
        let peer = import_peer(&r.ctx, &info)?;
        println!("[client] peer ({}, uasid 0x{:x}) jetty imported", seg.eid, seg.uasid);

        /* post receive buffer before sending ping: the pong always has a place to land (otherwise RNR) */
        let recv_sge = r.buf.sge(SCRATCH_OFF, MSG_SIZE as u32)?;
        r.jetty.post_recv(&[recv_sge], CLI_RECV_CTX)?;
        let send_sge = r.buf.sge(0, MSG_SIZE as u32)?;
        r.jetty.post_send(&peer, &[send_sge], CLI_SEND_CTX)?;

        /* wait only for recv completion: pong landed ⇒ ping already consumed
         * by the peer (dually, the peer learns from its own send completion
         * that the pong was consumed — both sides sync in-band; an earlier
         * local send completion is consumed and discarded by wait_cr) */
        wait_cr(&r.cq, CLI_RECV_CTX).await?;
        r.buf[SCRATCH_OFF..SCRATCH_OFF + MSG_SIZE].to_vec()
    };

    let end = cstr_len(&msg[..msg.len().min(MSG_SIZE)]);
    println!(
        "[client] pong via {}: \"{}\"",
        if hook { "tcp-hook" } else { "URMA SEND/RECV" },
        String::from_utf8_lossy(&msg[..end])
    );

    /* no bye: local side relies on its recv completion, the peer on its send completion; each tears down safely */
    Ok(())
}

/* ============================== entry ============================== */

/// Minimal URMA two-sided SEND/RECV demo (ping-pong), no out-of-band teardown.
#[derive(clap::Parser)]
#[command(
    name = "urma_pingpong",
    version,
    about = "Minimal URMA two-sided SEND/RECV demo (ping-pong), no out-of-band teardown.",
    after_help = "\
Usage (run on both nodes, each pointing at the other's IP):
  urma_pingpong -d <device> -i <peer_ip> [-p port] [-n name]   run server + client
Local test (no URMA device): add --tcp-hook, e.g. `./scripts/test_pingpong.sh`

The client retries connecting forever until the peer is up, so the two
nodes can be started in any order. Ctrl+C exits."
)]
struct Args {
    /// URMA device, e.g. udma2 / bonding_dev_0 (see examples/list_devices)
    #[arg(short, long = "dev")]
    dev_name: Option<String>,

    /// peer node IP (the node whose server we ping)
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
    let listen_port = args.listen_port.unwrap_or(args.port);
    let ping = fill_msg(&format!("ping from {name}"));
    let pong = fill_msg(&format!("pong from {name}"));
    combined(args, dev, &ping, &pong, listen_port).await
}

/// Server task (own resources + listen + one echo round) runs concurrently
/// with the client task. join! polls both on the same thread, so
/// UrmaRes with raw pointers needs no Send; URMA waits use async polling
/// (wait_cr) to yield, so axum is not starved. Client finishes on receiving
/// the pong, server on send completion — both sides sync via CQ completions.
async fn combined(
    args: &Args,
    dev: &str,
    ping: &[u8],
    pong: &[u8],
    listen_port: u16,
) -> Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);

    let srv = async {
        let res = res_opt(args.tcp_hook, "server", dev, pong)?;
        server_run(res, pong.to_vec(), listen_port, stop_rx).await
    };

    let cli = async {
        let http = reqwest::Client::new();
        let base = format!("http://{}:{}", args.peer_ip.as_deref().unwrap(), args.port);
        let r = match res_opt(args.tcp_hook, "client", dev, ping) {
            Ok(mut res) => client_run(&http, &base, res.as_mut(), args.tcp_hook, ping.to_vec()).await,
            Err(e) => Err(e),
        };
        if r.is_err() {
            let _ = stop_tx.send(true); /* tell the server task to exit so join! doesn't hang */
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
