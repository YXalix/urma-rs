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
//!   descriptor, `POST /import` / `POST /bye` receive peer notices; in URMA
//!   mode the data plane needs no CPU — the one-sided READ is done by the NIC
//!   straight from local registered memory.
//! - the memory side never notices being read, so after reading the client
//!   sends `POST /bye` and only then may the peer free its registered memory.
//! - `--tcp-hook`: emulates the data plane with a `GET /msg` request-reply for
//!   local logic testing without a URMA device; not one-sided.
//!
//! Usage (run on both nodes at once, each pointing at the other's IP; the client
//! retries forever, so start order doesn't matter):
//!   cargo run --example urma_hello -- -d bonding_dev_0 -i <peer_ip> -n nodeA
//! Local test (no device needed): ./scripts/test_hello.sh

use std::process::ExitCode;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use tokio::sync::{watch, Notify};

use urma_rs::error::Result;
use urma_rs::{Eid, Peer, TOKEN_VALUE};

#[path = "common/mod.rs"]
mod common;

use common::{
    cstr_len, default_name, exchange_desc, fill_msg, graceful_shutdown, http_err, report, res_opt,
    send_retry, PeerDesc, UrmaRes, MSG_SIZE, SCRATCH_OFF,
};

const DEFAULT_PORT: u16 = 13857;
const READ_CTX: u64 = 0x1234;

/* ============================== server (axum control plane) ============================== */

/// Server shared state: only Copy-able data such as descriptors/messages. The
/// UrmaRes itself is held by server_run (keeping the registered memory alive)
/// and stays out of axum State — it holds raw pointers and is not Send/Sync.
#[derive(Clone)]
struct SrvState {
    info: PeerDesc,
    msg: Vec<u8>,
    /// bye notice from the peer client (read done, we may free resources)
    done: Arc<Notify>,
}

async fn get_info(State(st): State<SrvState>) -> Json<PeerDesc> {
    Json(st.info)
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

/// server: holds `res` until exit (after the peer's bye) so the registered memory
/// stays alive while being read; `stop_rx` lets us exit early if this process's
/// client fails.
async fn server_run(
    res: Option<UrmaRes>,
    msg: Vec<u8>,
    port: u16,
    stop_rx: watch::Receiver<bool>,
) -> Result<()> {
    let info = res.as_ref().map(|r| r.desc()).unwrap_or_default();
    let done = Arc::new(Notify::new());
    let state = SrvState { info, msg, done: Arc::clone(&done) };
    let app = Router::new()
        .route("/info", get(get_info))
        .route("/msg", get(get_msg))
        .route("/import", post(post_import))
        .route("/bye", post(post_bye))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    println!("[server] listening on 0.0.0.0:{port}");

    axum::serve(listener, app)
        .with_graceful_shutdown(graceful_shutdown(Arc::clone(&done), stop_rx))
        .await?;
    Ok(())
}

/* ============================== client ============================== */

async fn client_run(
    http: &reqwest::Client,
    base: &str,
    res: Option<&mut UrmaRes>,
    hook: bool,
) -> Result<()> {
    /* control-plane handshake: fetch the peer descriptor and announce ourselves */
    let my = res.as_ref().map(|r| r.desc()).unwrap_or_default();
    let info = exchange_desc(http, base, my).await?;

    let msg: Vec<u8> = if hook {
        /* emulated data plane: request-reply */
        let b = send_retry(http, |c| c.get(format!("{base}/msg"))).await?;
        b.bytes().await.map_err(http_err)?.to_vec()
    } else {
        let r = res.expect("urma mode requires resources");
        let (seg, jetty) = info.to_pair();
        let p = Peer::import(&r.ctx, seg, jetty, TOKEN_VALUE)?;
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
Local test (no URMA device): add --tcp-hook, e.g. `./scripts/test_hello.sh`

The client retries connecting forever until the peer is up, so the two
nodes can be started in any order. Ctrl+C exits."
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
    let listen_port = args.listen_port.unwrap_or(args.port);
    let msg = fill_msg(&format!("hello world from {name}"));
    combined(args, dev, &msg, listen_port).await
}

/// Server task (own resources + listen + serve once) and client task run
/// concurrently. join! polls both on one thread, so the raw-pointer UrmaRes
/// never needs Send; after the client reads and sends bye, wait for the
/// server's peer-bye before wrapping up.
async fn combined(args: &Args, dev: &str, msg: &[u8], listen_port: u16) -> Result<()> {
    let (stop_tx, stop_rx) = watch::channel(false);

    let srv = async {
        let res = res_opt(args.tcp_hook, "server", dev, msg)?;
        server_run(res, msg.to_vec(), listen_port, stop_rx).await
    };

    let cli = async {
        let http = reqwest::Client::new();
        let base = format!("http://{}:{}", args.peer_ip.as_deref().unwrap(), args.port);
        let r = match res_opt(args.tcp_hook, "client", dev, msg) {
            Ok(mut res) => client_run(&http, &base, res.as_mut(), args.tcp_hook).await,
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
