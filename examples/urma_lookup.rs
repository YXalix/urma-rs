//! urma_lookup — P2P metadata addressing example, single-file example.
//!
//! Topology: master (axum HTTP service, uses no URMA device) hands out the record
//! directory; clients read from each other peer-to-peer, the data plane bypasses
//! the master. Protocol flow: register -> ack -> ready barrier -> directory ->
//! fetch all records -> done barrier -> exit; barriers use `tokio::sync::Barrier`,
//! control-plane messages are HTTP JSON.
//!
//! The data plane is abstracted by [`Transport`], with two implementations:
//! URMA (real one-sided READ) and HTTP hook (local logic test, no device needed).
//!
//! Usage:
//!   cargo run --example urma_lookup -- --master --clients 2
//!   cargo run --example urma_lookup -- -d bonding_dev_0 -m <master_ip> -n nodeA
//!   cargo run --example urma_lookup -- --tcp-hook -m 127.0.0.1 -n nodeA
//! Or run ./scripts/test_local.sh for a local logic test.

use std::net::{Ipv4Addr, SocketAddr};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use clap::Parser;
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Barrier};

use urma_rs::error::{Error, Result};
use urma_rs::{
    CompletionQueue, Context, Jetty, JettyOpts, Peer, RegisteredBuf, Urma, DEFAULT_DEPTH,
    PAGE_SIZE, TOKEN_VALUE,
};

#[path = "common/mod.rs"]
mod common;

use common::{
    check_loopback, check_mode_support, cstr_len, default_name, http_err, import_peer, json_retry,
    report, send_retry, PeerDesc, MSG_SIZE,
};

/* ============================== Constants and memory layout ============================== */

const MAX_CLIENTS: usize = 16;
const MAX_RECORDS_PER_CLIENT: u32 = 64;
const DEFAULT_PORT: u16 = 13858;
const DEFAULT_CLIENTS: usize = 2;
const DEFAULT_RECORDS: u32 = 4;

/*
 * Local memory layout (shared by both transports):
 *   [0, my_cnt * MSG_SIZE)   own records (may be read by peers at any time)
 *   [SCRATCH_OFF, +MSG_SIZE) landing buffer for records read from peers
 * scratch must never overlap own records, or it would clobber data being read.
 */
const SCRATCH_OFF: usize = MAX_RECORDS_PER_CLIENT as usize * MSG_SIZE;
const fn buf_len() -> usize {
    (SCRATCH_OFF + MSG_SIZE).div_ceil(PAGE_SIZE) * PAGE_SIZE
}

/* ============================== Control-plane messages (JSON) ============================== */

/// Directory entry: owner of a record range
#[derive(Clone, Serialize, Deserialize)]
struct OwnerInfo {
    name: String,
    first_record_id: u32,
    record_cnt: u32,
    desc: PeerDesc,
    /// tcp-hook only: client source IPv4 (master learns it from the connection)
    peer_addr: [u8; 4],
    /// tcp-hook only: client data-service port (0 in URMA mode)
    data_port: u32,
}

/// client -> master register message
#[derive(Serialize, Deserialize)]
struct RegisterMsg {
    name: String,
    record_cnt: u32,
    desc: PeerDesc,
    data_port: u32,
}

/// master -> client register ack
#[derive(Serialize, Deserialize)]
struct RegisterAck {
    first_record_id: u32,
}

/// master -> client directory
#[derive(Serialize, Deserialize)]
struct Directory {
    owners: Vec<OwnerInfo>,
}

/* ============================== Transport abstraction ============================== */

/// Data-plane hook: client application logic depends only on this interface.
/// `read_record` is async — the tcp-hook implementation uses HTTP; the URMA
/// implementation polls synchronously over FFI internally (a single READ
/// completes in milliseconds; the demo just blocks the current worker).
#[allow(async_fn_in_trait)] /* used generically inside this example only, not exported */
trait Transport {
    /// Set up local resources (URMA: context/jfc/jfr/jetty/seg; tcp-hook: axum task on the data port)
    fn init(&mut self) -> Result<()>;

    /// Data-service port in tcp-hook mode (0 in URMA mode)
    fn data_port(&self) -> u32 {
        0
    }

    /// Local data-plane descriptor, sent in the register message
    fn desc(&self) -> PeerDesc;

    /// Callback after master assigns the range (tcp-hook server needs the range)
    fn set_first(&mut self, _first: u32) {}

    /// Access local memory (for writing own records; layout per SCRATCH_OFF comment)
    fn with_buf(&mut self, f: &mut dyn FnMut(&mut [u8]));

    /// Import an owner's resources by directory index
    fn import(&mut self, idx: usize, o: &OwnerInfo) -> Result<()>;

    /// Read one record, returning MSG_SIZE bytes
    async fn read_record(&mut self, idx: usize, o: &OwnerInfo, rid: u32) -> Result<Vec<u8>>;
}

/* ============================== transport: URMA ============================== */

/// Real URMA one-sided READ. Children hold handles to their parents, so drop
/// order is safe regardless of field order; not Send/Sync.
struct UrmaTransport {
    ctx: Context,
    cq: CompletionQueue,
    jetty: Jetty,
    buf: RegisteredBuf,
    /// exported import blobs (urma_get_seg_ctx / urma_get_rjetty); on bonding
    /// they let the peer import without the kernel-side exchange
    seg_ctx: Vec<u8>,
    rjetty: Vec<u8>,
    peers: Vec<Option<Peer>>,
    _urma: Urma,
}

impl UrmaTransport {
    fn new(dev_name: &str) -> Result<Self> {
        check_mode_support(dev_name)?;
        let urma = Urma::init()?;
        let ctx = Context::create(&urma, dev_name)?;
        println!("[client] use device {dev_name} eid ({})", ctx.eid());
        let cq = CompletionQueue::new(&ctx, DEFAULT_DEPTH)?;
        let jetty = Jetty::new(
            &ctx,
            &cq,
            JettyOpts { multi_path: dev_name.starts_with("bonding"), ..Default::default() },
        )?;
        let buf = RegisteredBuf::new(&ctx, buf_len(), TOKEN_VALUE)?;
        let seg_ctx = buf.export_seg_ctx()?;
        let rjetty = jetty.export_rjetty()?;
        Ok(UrmaTransport { ctx, cq, jetty, buf, seg_ctx, rjetty, peers: Vec::new(), _urma: urma })
    }
}

impl Transport for UrmaTransport {
    fn init(&mut self) -> Result<()> {
        Ok(()) /* resources already set up in new() */
    }

    fn desc(&self) -> PeerDesc {
        PeerDesc::of(
            self.buf.descriptor(),
            self.jetty.id(),
            self.seg_ctx.clone(),
            self.rjetty.clone(),
        )
    }

    fn with_buf(&mut self, f: &mut dyn FnMut(&mut [u8])) {
        f(&mut self.buf[..]);
    }

    fn import(&mut self, idx: usize, o: &OwnerInfo) -> Result<()> {
        check_loopback(&self.desc(), &o.desc)?;
        if self.peers.len() <= idx {
            self.peers.resize_with(idx + 1, || None);
        }
        let peer = import_peer(&self.ctx, &o.desc)?;
        self.peers[idx] = Some(peer);
        Ok(())
    }

    async fn read_record(&mut self, idx: usize, o: &OwnerInfo, rid: u32) -> Result<Vec<u8>> {
        let remote_va = o.desc.seg_va + u64::from(rid - o.first_record_id) * MSG_SIZE as u64;
        let user_ctx = u64::from(rid); /* record id as wr context, checked at poll */
        let peer = self.peers[idx]
            .as_ref()
            .ok_or_else(|| Error::Invalid("owner not imported".into()))?;
        let sge = self.buf.sge(SCRATCH_OFF, MSG_SIZE as u32)?;
        self.jetty.post_read(peer, remote_va, &[sge], user_ctx)?;
        self.cq.wait_read(user_ctx)?;
        let mut out = vec![0u8; MSG_SIZE];
        out.copy_from_slice(&self.buf[SCRATCH_OFF..SCRATCH_OFF + MSG_SIZE]);
        Ok(out)
    }
}

/* ============================== transport: HTTP hook ============================== */

/// Shared state of the tcp-hook data service
#[derive(Clone)]
struct HookState {
    buf: Arc<Mutex<Vec<u8>>>,
    my_first: Arc<AtomicU32>,
    record_cnt: u32,
}

/// Owner side: one request per record — GET /record/{rid}, reply with the 64-byte record
async fn hook_record(State(st): State<HookState>, Path(rid): Path<u32>) -> impl IntoResponse {
    let first = st.my_first.load(Ordering::Relaxed);
    if rid >= first && rid < first + st.record_cnt {
        let buf = st.buf.lock().unwrap();
        let off = (rid - first) as usize * MSG_SIZE;
        (StatusCode::OK, buf[off..off + MSG_SIZE].to_vec())
    } else {
        (StatusCode::NOT_FOUND, Vec::new())
    }
}

/// HTTP-emulated data plane: only for verifying orchestration logic on machines
/// without a URMA device; not one-sided (the owner's CPU moves the data).
struct TcpHookTransport {
    record_cnt: u32,
    my_first: Arc<AtomicU32>,
    buf: Arc<Mutex<Vec<u8>>>,
    stop: Option<watch::Sender<bool>>,
    port: u16,
    http: reqwest::Client,
}

impl TcpHookTransport {
    fn new(record_cnt: u32) -> Self {
        TcpHookTransport {
            record_cnt,
            my_first: Arc::new(AtomicU32::new(0)),
            buf: Arc::new(Mutex::new(vec![0u8; buf_len()])),
            stop: None,
            port: 0,
            http: reqwest::Client::new(),
        }
    }
}

impl Transport for TcpHookTransport {
    fn init(&mut self) -> Result<()> {
        /* start the axum data service on a random port; from_std must be called
           within a runtime context (init is always entered from async run_client) */
        let std_l = std::net::TcpListener::bind(("0.0.0.0", 0))?;
        self.port = std_l.local_addr()?.port();
        std_l.set_nonblocking(true)?; /* required to register with the tokio reactor */
        let listener = tokio::net::TcpListener::from_std(std_l)?;

        let (stop_tx, mut stop_rx) = watch::channel(false);
        let state = HookState {
            buf: Arc::clone(&self.buf),
            my_first: Arc::clone(&self.my_first),
            record_cnt: self.record_cnt,
        };
        let app = Router::new().route("/record/{rid}", get(hook_record)).with_state(state);
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = stop_rx.changed().await;
                })
                .await;
        });
        self.stop = Some(stop_tx);
        println!(
            "[client] tcp-hook mode: data plane served on HTTP port {} (logic test only, no URMA device)",
            self.port
        );
        Ok(())
    }

    fn data_port(&self) -> u32 {
        u32::from(self.port)
    }

    fn desc(&self) -> PeerDesc {
        PeerDesc::default() /* URMA descriptor unused in hook mode */
    }

    fn set_first(&mut self, first: u32) {
        self.my_first.store(first, Ordering::Relaxed);
    }

    fn with_buf(&mut self, f: &mut dyn FnMut(&mut [u8])) {
        f(&mut self.buf.lock().unwrap()[..]);
    }

    fn import(&mut self, _idx: usize, o: &OwnerInfo) -> Result<()> {
        if o.data_port == 0 {
            return Err(Error::Invalid(format!("owner '{}' has no data port", o.name)));
        }
        Ok(()) /* fetch on demand, no pre-imported resources */
    }

    async fn read_record(&mut self, _idx: usize, o: &OwnerInfo, rid: u32) -> Result<Vec<u8>> {
        let url = format!("http://{}:{}/record/{rid}", Ipv4Addr::from(o.peer_addr), o.data_port);
        let resp = self.http.get(&url).send().await.map_err(http_err)?;
        if !resp.status().is_success() {
            return Err(Error::Invalid(format!(
                "fetch record {rid} from '{}': http status {}",
                o.name,
                resp.status()
            )));
        }
        let mut out = vec![0u8; MSG_SIZE];
        let b = resp.bytes().await.map_err(http_err)?;
        if b.len() != MSG_SIZE {
            return Err(Error::Invalid(format!("record {rid}: bad body len {}", b.len())));
        }
        out.copy_from_slice(&b);
        Ok(out)
    }
}

impl Drop for TcpHookTransport {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(true); /* data-service task exits on its own, no need to wait */
        }
    }
}

/* ============================== master ============================== */

struct MasterCfg {
    port: u16,
    expect: usize,
}

#[derive(Clone)]
struct MasterState {
    clients: Arc<Mutex<Vec<OwnerInfo>>>,
    next_rid: Arc<AtomicU32>,
    ready: Arc<Barrier>,
    done: Arc<Barrier>,
    shutdown: watch::Sender<bool>,
}

/// 1. Register: assign a record id range and reply immediately
async fn m_register(
    State(st): State<MasterState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(reg): Json<RegisterMsg>,
) -> std::result::Result<Json<RegisterAck>, (StatusCode, String)> {
    if reg.record_cnt == 0 || reg.record_cnt > MAX_RECORDS_PER_CLIENT {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("bad register from '{}' (cnt {})", reg.name, reg.record_cnt),
        ));
    }
    let peer_addr = match peer.ip() {
        std::net::IpAddr::V4(v4) => v4.octets(),
        std::net::IpAddr::V6(_) => [0, 0, 0, 0],
    };
    let first = st.next_rid.fetch_add(reg.record_cnt, Ordering::SeqCst);
    let info = OwnerInfo {
        name: reg.name.clone(),
        first_record_id: first,
        record_cnt: reg.record_cnt,
        desc: reg.desc,
        peer_addr,
        data_port: reg.data_port,
    };
    println!(
        "[master] client '{}' registered: records [{}, {})",
        info.name, info.first_record_id, info.first_record_id + info.record_cnt
    );
    st.clients.lock().unwrap().push(info);
    Ok(Json(RegisterAck { first_record_id: first }))
}

/// 2. ready barrier: release only after everyone has written their records;
///    reply with the full directory (avoids reading half-written data)
async fn m_ready(State(st): State<MasterState>) -> Json<Directory> {
    let leader = st.ready.wait().await.is_leader();
    let owners = st.clients.lock().unwrap().clone();
    if leader {
        let total: u32 = owners.iter().map(|o| o.record_cnt).sum();
        println!(
            "[master] directory broadcast: {} record(s) across {} owner(s)",
            total,
            owners.len()
        );
    }
    Json(Directory { owners })
}

/// 3. done barrier: everyone finished peer-to-peer reads, release together —
///    only now is it safe to destroy local resources; last arrival shuts down master
async fn m_done(State(st): State<MasterState>) -> StatusCode {
    let leader = st.done.wait().await.is_leader();
    if leader {
        println!("[master] all clients finished, bye");
        let _ = st.shutdown.send(true);
    }
    StatusCode::NO_CONTENT
}

/// master uses HTTP only and never calls any URMA API; deployable on any node.
async fn run_master(cfg: &MasterCfg) -> Result<()> {
    if cfg.expect == 0 || cfg.expect > MAX_CLIENTS {
        return Err(Error::Invalid(format!("expect must be in [1, {MAX_CLIENTS}]")));
    }
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", cfg.port)).await?;
    println!(
        "[master] expecting {} client(s), listening on 0.0.0.0:{}",
        cfg.expect, cfg.port
    );

    let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
    let state = MasterState {
        clients: Arc::new(Mutex::new(Vec::new())),
        next_rid: Arc::new(AtomicU32::new(0)),
        ready: Arc::new(Barrier::new(cfg.expect)),
        done: Arc::new(Barrier::new(cfg.expect)),
        shutdown: shutdown_tx,
    };
    let app = Router::new()
        .route("/register", post(m_register))
        .route("/ready", post(m_ready))
        .route("/done", post(m_done))
        .with_state(state);

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await?;
    Ok(())
}

/* ============================== client ============================== */

struct ClientCfg {
    master_ip: String,
    port: u16,
    name: String,
    records: u32,
}

/// client flow: register -> write records -> ready/directory -> import -> fetch all -> done -> exit
async fn run_client<T: Transport>(
    cfg: &ClientCfg,
    http: &reqwest::Client,
    tr: &mut T,
) -> Result<()> {
    tr.init()?;
    let base = format!("http://{}:{}", cfg.master_ip, cfg.port);

    /* 1. Register: the ack carries the start of the assigned record range */
    let reg = RegisterMsg {
        name: cfg.name.clone(),
        record_cnt: cfg.records,
        desc: tr.desc(),
        data_port: tr.data_port(),
    };
    let ack: RegisterAck =
        json_retry(http, |c| c.post(format!("{base}/register")).json(&reg)).await?;
    let my_first = ack.first_record_id;
    tr.set_first(my_first);

    /* 2. Write records into local memory per the assigned range */
    tr.with_buf(&mut |buf| {
        for i in 0..cfg.records as usize {
            let s = format!("record {} from {}", my_first + i as u32, cfg.name);
            let off = i * MSG_SIZE;
            buf[off..off + MSG_SIZE].fill(0);
            let n = s.len().min(MSG_SIZE - 1);
            buf[off..off + n].copy_from_slice(&s.as_bytes()[..n]);
        }
        let last = (cfg.records as usize - 1) * MSG_SIZE;
        println!(
            "[client] registered, holding records [{}, {}): \"{}\" ... \"{}\"",
            my_first,
            my_first + cfg.records,
            String::from_utf8_lossy(&buf[..cstr_len(&buf[..MSG_SIZE])]),
            String::from_utf8_lossy(&buf[last..last + cstr_len(&buf[last..last + MSG_SIZE])])
        );
    });

    /* 3. ready barrier: master replies with the directory once everyone is ready */
    let dir: Directory = json_retry(http, |c| c.post(format!("{base}/ready"))).await?;
    let owners = dir.owners;
    let total: u32 = owners.iter().map(|o| o.record_cnt).sum();
    println!(
        "[client] directory received: {} owner(s), {} record(s) in total",
        owners.len(),
        total
    );

    /* 4. Import other owners' resources (skip own entry) */
    for (idx, o) in owners.iter().enumerate() {
        if o.first_record_id == my_first && o.record_cnt == cfg.records {
            continue; /* this is myself */
        }
        tr.import(idx, o)?;
        println!(
            "[client] owner '{}' (records [{}, {})) ready to fetch",
            o.name, o.first_record_id, o.first_record_id + o.record_cnt
        );
    }

    /* 5. Walk all records: skip own, read others' peer-to-peer */
    for rid in 0..total {
        let idx = owners
            .iter()
            .position(|o| rid >= o.first_record_id && rid < o.first_record_id + o.record_cnt)
            .ok_or_else(|| Error::Invalid(format!("record {rid} has no owner")))?;
        let o = &owners[idx];
        if rid >= my_first && rid < my_first + cfg.records {
            continue; /* own records were already printed at registration */
        }
        let data = tr.read_record(idx, o, rid).await?;
        let end = cstr_len(&data);
        println!(
            "[client] fetched record {} from {}: \"{}\"",
            rid,
            o.name,
            String::from_utf8_lossy(&data[..end])
        );
    }

    /* 6. done barrier: master releases after everyone finishes; resources must stay alive until then */
    send_retry(http, |c| c.post(format!("{base}/done"))).await?;
    println!("[client] released by master, bye");
    Ok(())
}

/* ============================== Entry ============================== */

/// URMA metadata-lookup demo: an HTTP-only master maps record ids to clients,
/// clients fetch records from each other directly via one-sided URMA READ.
#[derive(clap::Parser)]
#[command(
    name = "urma_lookup",
    version,
    about = "URMA metadata-lookup demo: an HTTP-only master maps record ids to clients, \
             clients fetch records from each other directly via one-sided URMA READ.",
    after_help = "\
Usage:
  master: urma_lookup --master [-c clients] [-p port]
  client: urma_lookup [-d <device> | --tcp-hook] -m <master_ip> [-r records] [-p port] [-n name]
Local test (no URMA device): add --tcp-hook, e.g. `./scripts/test_local.sh`

Clients retry connecting to the master forever, so start order is free.
Ctrl+C exits."
)]
struct Args {
    /// run as master (no URMA device needed)
    #[arg(short = 'M', long = "master")]
    as_master: bool,

    /// emulate the data plane over HTTP (local logic test only)
    #[arg(short = 'T', long = "tcp-hook")]
    tcp_hook: bool,

    /// expected number of clients (master only)
    #[arg(short, long, default_value_t = DEFAULT_CLIENTS)]
    clients: usize,

    /// URMA device, e.g. udma2 / bonding_dev_0 (client only)
    #[arg(short, long = "dev")]
    dev_name: Option<String>,

    /// master IP (client only)
    #[arg(short = 'm', long = "master-ip")]
    master_ip: Option<String>,

    /// records held by this client, in [1, 64]
    #[arg(short, long, default_value_t = DEFAULT_RECORDS)]
    records: u32,

    /// master HTTP port
    #[arg(short, long, default_value_t = DEFAULT_PORT)]
    port: u16,

    /// identity in records (default: hostname)
    #[arg(short, long)]
    name: Option<String>,
}

impl Args {
    /// Cross-field constraints
    fn validate(&self) -> std::result::Result<(), String> {
        if self.records == 0 || self.records > MAX_RECORDS_PER_CLIENT {
            return Err(format!("records per client must be in [1, {MAX_RECORDS_PER_CLIENT}]"));
        }
        if !self.as_master && self.master_ip.is_none() {
            return Err("client mode requires -m <master_ip>".into());
        }
        if !self.as_master && !self.tcp_hook && self.dev_name.is_none() {
            return Err("URMA mode requires -d <device> (or use --tcp-hook)".into());
        }
        Ok(())
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    if let Err(m) = args.validate() {
        eprintln!("error: {m}\n\nFor more information, try '--help'.");
        return ExitCode::from(2);
    }
    let name = default_name(args.name.as_deref());

    let result = if args.as_master {
        run_master(&MasterCfg { port: args.port, expect: args.clients }).await
    } else {
        let master_ip = args.master_ip.clone().expect("validated by Args::validate");
        let cfg = ClientCfg { master_ip, port: args.port, name, records: args.records };
        let http = reqwest::Client::new();
        if args.tcp_hook {
            let mut tr = TcpHookTransport::new(cfg.records);
            run_client(&cfg, &http, &mut tr).await
        } else {
            let dev = args.dev_name.clone().expect("validated by Args::validate");
            match UrmaTransport::new(&dev) {
                Ok(mut tr) => run_client(&cfg, &http, &mut tr).await,
                Err(e) => Err(e),
            }
        }
    };
    report(result)
}
