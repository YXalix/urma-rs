//! READ-semantics latency benchmark: URMA one-sided READ vs the closest TCP
//! equivalent, swept over message sizes.
//!
//! TCP has no one-sided semantics, so "read N bytes of the peer's memory"
//! degenerates to a request/response round trip: the client sends a 4-byte
//! LE length, the server answers with N bytes from a pattern-filled buffer —
//! one RTT per op, the same emulation netperf's request/response mode uses.
//! The URMA side measures the real thing: `post_read` then busy-poll of the
//! completion queue; the server's CPU is never involved, which is exactly
//! the semantic gap the two columns are meant to show.
//!
//! Fairness rules shared by both transports: identical size sweep; per size
//! one verify pass (pattern check, outside the timed loop) + `--warmup`
//! untimed iterations, then `--iters` timed ones; every sample covers the
//! full client-side path including syscalls; ops are strictly serialized
//! (post → completion / request → full response). TCP_NODELAY is forced on
//! both ends (Nagle + delayed ACK would add ~40ms artifacts).
//!
//! serve-urma/read-urma follow urma_cli's pure flow (the descriptor travels
//! as one hand-packed hex line, imports use the export blobs; defaults are
//! CTP-RM like every example):
//!
//! ```bash
//! # URMA READ latency between two nodes:
//! nodeA$ cargo run --example read_lat -- serve-urma -d bonding_dev_0 --sizes 8..16m
//! nodeB$ cargo run --example read_lat -- read-urma -d bonding_dev_0 '<[desc] hex>' \
//!         --sizes 8..16m   # serve sizes its buffer to the sweep's maximum
//!
//! # TCP reference over the same pair of machines:
//! nodeA$ cargo run --example read_lat -- serve-tcp
//! nodeB$ cargo run --example read_lat -- read-tcp --addr <ipA>
//! ```
//!
//! `scripts/test_readlat.sh` automates the matrix: TCP loopback as a local
//! smoke test, plus both transports across a UB_NODES pair. `--csv` prints
//! `csv,transport,size_bytes,...` rows for plotting instead of the table.

use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use urma_rs::{
    query_device, Completion, CompletionQueue, Context, DeviceCap, Eid, Error, Jetty, JettyOpts,
    Peer, RegisteredBuf, Result, SegDesc, TpType, TransMode, Urma, DEFAULT_DEPTH, TOKEN_VALUE,
};

/// user_ctx tag for every READ (strictly one outstanding op at a time)
const READ_CTX: u64 = 0x1a7;
/// busy-poll deadline per READ: generous, only a guard against a hung fabric
const READ_TIMEOUT: Duration = Duration::from_secs(30);
/// default size sweep, bytes: small-packet latency through the bandwidth regime
const DEFAULT_SIZES: &str = "8,64,256,1024,4096,16384,65536,262144,1048576";
/// TCP connect retry budget: CONNECT_TRIES x CONNECT_INTERVAL (start-order freedom)
const CONNECT_TRIES: u32 = 50;
const CONNECT_INTERVAL: Duration = Duration::from_millis(200);
/// default serve-side buffer, bytes (raise --buf-len to sweep larger sizes)
const DEFAULT_BUF_LEN: usize = 1048576;

#[derive(Parser)]
#[command(
    name = "read_lat",
    version,
    about = "READ-semantics latency benchmark: URMA one-sided READ vs TCP request/response, \
             swept over message sizes (min/p50/avg/p99/max per size)",
    after_help = "READ semantics comparison: a URMA READ is one-sided (the server CPU sleeps); \
                  TCP has no one-sided op, so a read is emulated as a 4-byte length request + \
                  N-byte response round trip. Both transports share the sweep, the verify pass \
                  and the warmup. scripts/test_readlat.sh runs the full matrix."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// URMA serve: register pattern-filled memory, print the descriptor, hold it
    ServeUrma(ServeUrmaArgs),
    /// URMA client: import the peer's descriptor, timed one-sided READs per size
    ReadUrma(ReadUrmaArgs),
    /// TCP reference serve: answer length requests with pattern bytes
    ServeTcp(ServeTcpArgs),
    /// TCP reference client: timed request/response round trips per size
    ReadTcp(ReadTcpArgs),
}

/// mode flags shared by serve-urma/read-urma (trans_mode is set at create,
/// tp_type at import; both peers must run the same combination)
#[derive(Args)]
struct ModeArgs {
    /// device name, as printed by list_devices
    #[arg(short, long)]
    dev: String,
    /// transport mode of the jfr/jfs
    #[arg(long, default_value = "rm")]
    mode: ModeArg,
    /// tp type the reading side chooses at import
    #[arg(long, default_value = "ctp")]
    tp: TpArg,
    /// multi-path (bonding devices force it on unless the cap probe says no)
    #[arg(long)]
    multi_path: bool,
}

/// benchmark knobs shared by read-urma/read-tcp
#[derive(Args)]
struct BenchArgs {
    /// read sizes: comma list (8,64,1k) or doubling range first..last (8..16m);
    /// k/m/g suffixes allowed, deduped, swept ascending
    #[arg(long, default_value = DEFAULT_SIZES)]
    sizes: String,
    /// timed iterations per size
    #[arg(long, default_value_t = 1000)]
    iters: u32,
    /// untimed iterations per size, before the timed loop
    #[arg(long, default_value_t = 100)]
    warmup: u32,
    /// print csv,transport,size_bytes,iters,min_us,p50_us,avg_us,p99_us,max_us rows
    #[arg(long)]
    csv: bool,
}

#[derive(Args)]
struct ServeUrmaArgs {
    #[command(flatten)]
    mode: ModeArgs,
    /// size sweep the reader will run; the segment is sized to its maximum
    #[arg(long)]
    sizes: Option<String>,
    /// explicit buffer size in bytes (overrides --sizes-derived sizing)
    #[arg(long)]
    buf_len: Option<usize>,
}

#[derive(Args)]
struct ReadUrmaArgs {
    #[command(flatten)]
    mode: ModeArgs,
    /// the [desc] hex line printed by the peer's serve-urma ('-' reads one line from stdin)
    desc: String,
    #[command(flatten)]
    bench: BenchArgs,
}

#[derive(Args)]
struct ServeTcpArgs {
    /// listen address
    #[arg(long, default_value = "0.0.0.0")]
    addr: String,
    /// listen port
    #[arg(short, long, default_value_t = 13860)]
    port: u16,
    /// size sweep the client will run; the buffer is sized to its maximum
    #[arg(long)]
    sizes: Option<String>,
    /// explicit buffer size in bytes (overrides --sizes-derived sizing)
    #[arg(long)]
    buf_len: Option<usize>,
}

#[derive(Args)]
struct ReadTcpArgs {
    /// server address
    #[arg(short, long, default_value = "127.0.0.1")]
    addr: String,
    /// server port
    #[arg(short, long, default_value_t = 13860)]
    port: u16,
    #[command(flatten)]
    bench: BenchArgs,
}

#[derive(Clone, Copy, ValueEnum)]
enum ModeArg {
    Rm,
    Rc,
    Um,
}

impl ModeArg {
    fn get(self) -> TransMode {
        match self {
            ModeArg::Rm => TransMode::Rm,
            ModeArg::Rc => TransMode::Rc,
            ModeArg::Um => TransMode::Um,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum TpArg {
    Rtp,
    Ctp,
    Utp,
}

impl TpArg {
    fn get(self) -> TpType {
        match self {
            TpArg::Rtp => TpType::Rtp,
            TpArg::Ctp => TpType::Ctp,
            TpArg::Utp => TpType::Utp,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let r = match &cli.cmd {
        Cmd::ServeUrma(a) => serve_urma_run(a),
        Cmd::ReadUrma(a) => read_urma_run(a),
        Cmd::ServeTcp(a) => serve_tcp_run(a),
        Cmd::ReadTcp(a) => read_tcp_run(a),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[main] error: {e}");
            ExitCode::FAILURE
        }
    }
}

/* ============================== urma: serve ============================== */

fn serve_urma_run(a: &ServeUrmaArgs) -> Result<()> {
    let (mode, _tp, multi_path, _) = preflight(&a.mode)?;
    let buf_len = resolve_buf_len(a.buf_len, a.sizes.as_deref())?;

    println!("[1/5] urma init + context on {}", a.mode.dev);
    let urma = Urma::init()?;
    let ctx = Context::create(&urma, &a.mode.dev)?;
    println!("      context eid {}", ctx.eid());

    println!("[2/5] completion queue (depth {DEFAULT_DEPTH}) + jetty");
    let cq = CompletionQueue::new(&ctx, DEFAULT_DEPTH)?;
    let jetty =
        Jetty::new(&ctx, &cq, JettyOpts { trans_mode: mode, multi_path, ..Default::default() })?;
    println!("      jetty id {} uasid {:#x}", jetty.id().id, jetty.id().uasid);

    println!("[3/5] register {buf_len}-byte segment, fill read pattern");
    let mut buf = RegisteredBuf::new(&ctx, buf_len, TOKEN_VALUE)?;
    fill_pat(&mut buf[..]);
    let seg = buf.descriptor();
    println!(
        "      seg va {:#x} len {} token_id {} attr {:#x}",
        seg.va, seg.len, seg.token_id, seg.attr
    );

    println!("[4/5] export seg-ctx / rjetty blobs (blob import path, no kernel exchange)");
    let seg_ctx = buf.export_seg_ctx()?;
    let rjetty = jetty.export_rjetty()?;
    println!("      seg-ctx {} bytes, rjetty {} bytes", seg_ctx.len(), rjetty.len());

    println!("[5/5] descriptor for the peer (one hex line):");
    println!("[desc] {}", pack_desc(&WireDesc { seg, seg_ctx, rjetty }));

    /* one-sided READs never involve this CPU and raise no completion here;
       the only remaining job is keeping the resources alive */
    if std::io::stdin().is_terminal() {
        println!("[serve-urma] holding the segment for the peer's READs - press Enter to exit");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    } else {
        println!("[serve-urma] holding the segment for the peer's READs (stdin not a tty: park until killed)");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    println!("[serve-urma] bye");
    Ok(())
}

/* ============================== urma: read =============================== */

fn read_urma_run(a: &ReadUrmaArgs) -> Result<()> {
    let mut sizes = bench_sizes(&a.bench)?;

    println!("[read-urma] unpack peer descriptor");
    let desc_hex = if a.desc == "-" {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(Error::Io)?;
        line
    } else {
        a.desc.clone()
    };
    let wire = unpack_desc(&desc_hex)?;
    println!(
        "      peer seg eid {} uasid {:#x} va {:#x} len {}",
        wire.seg.eid, wire.seg.uasid, wire.seg.va, wire.seg.len
    );

    let (mode, tp, multi_path, cap) = preflight(&a.mode)?;
    /* sizes above the READ ceiling are skipped with a note, not fatal: one
       sweep then works on any device. A one-sided READ is bounded by the
       device's max_read_size — max_msg_size caps two-sided messages, not
       READs (urma_device_cap_t carries both); 0 = not reported, then fall
       back to max_msg_size, then to no limit */
    let read_cap = if cap.max_read_size != 0 {
        cap.max_read_size
    } else if cap.max_msg_size != 0 {
        cap.max_msg_size
    } else {
        u64::MAX
    };
    let limit = read_cap.min(wire.seg.len);
    sizes.retain(|&s| {
        let ok = (s as u64) <= limit;
        if !ok {
            println!(
                "[read-urma] skip size {s}: above the {limit}-byte READ ceiling (peer segment {}, device max_read_size {} / max_msg_size {})",
                wire.seg.len, cap.max_read_size, cap.max_msg_size
            );
        }
        ok
    });
    if sizes.is_empty() {
        return Err(Error::Invalid("no size left after the device/segment limits".into()));
    }

    println!("[1/4] urma init + context on {}", a.mode.dev);
    let urma = Urma::init()?;
    let ctx = Context::create(&urma, &a.mode.dev)?;
    println!("      context eid {}", ctx.eid());
    if wire.seg.eid == ctx.eid() {
        return Err(Error::Invalid(
            "peer eid equals this context's eid: single-machine loopback crashes inside \
             liburma's import path; run serve-urma on another node"
                .into(),
        ));
    }

    println!("[2/4] own completion queue + jetty (the READ is posted from our jetty)");
    let cq = CompletionQueue::new(&ctx, DEFAULT_DEPTH)?;
    let jetty =
        Jetty::new(&ctx, &cq, JettyOpts { trans_mode: mode, multi_path, ..Default::default() })?;
    println!("      jetty id {} uasid {:#x}", jetty.id().id, jetty.id().uasid);

    println!("[3/4] import peer via blobs ({tp})");
    let peer = Peer::import_ctx(&ctx, &wire.seg_ctx, &wire.rjetty, tp, TOKEN_VALUE)?;

    let max_size = *sizes.last().unwrap();
    println!(
        "[4/4] register landing buffer ({max_size} bytes), sweep {} sizes: {}",
        sizes.len(),
        sizes.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(",")
    );
    let landing = RegisteredBuf::new(&ctx, max_size, TOKEN_VALUE)?;
    let remote_va = wire.seg.va;

    print_header("urma", a.bench.iters, a.bench.warmup, a.bench.csv);
    for &size in &sizes {
        let sge = landing.sge(0, size as u32)?;
        /* verify pass outside the timed loop: one READ, compare the pattern */
        jetty.post_read(&peer, remote_va, &[sge], READ_CTX)?;
        let _ = wait_read_spin(&cq)?;
        check_pat(&format!("size {size} verify read"), &landing[..size])?;
        let s = run_iters(
            || {
                jetty.post_read(&peer, remote_va, &[sge], READ_CTX)?;
                wait_read_spin(&cq).map(|_| ())
            },
            a.bench.warmup,
            a.bench.iters,
        )?;
        print_row("urma", size, a.bench.iters, &s, a.bench.csv);
    }
    println!("[read-urma] done: {} sizes, {} iters each", sizes.len(), a.bench.iters);
    Ok(())
}

/// busy-poll the CQ until the READ_CTX completion lands. `wait_read` sleeps
/// 100ms between polls — fine for demos, useless for a latency measurement:
/// here we spin (with a hard deadline as a hang guard) instead.
fn wait_read_spin(cq: &CompletionQueue) -> Result<Completion> {
    let deadline = Instant::now() + READ_TIMEOUT;
    loop {
        if let Some(cr) = cq.poll()? {
            if !cr.is_success() || cr.user_ctx != READ_CTX {
                return Err(Error::BadCompletion { status: cr.status, user_ctx: cr.user_ctx });
            }
            return Ok(cr);
        }
        if Instant::now() >= deadline {
            return Err(Error::PollTimeout { user_ctx: READ_CTX });
        }
        std::hint::spin_loop();
    }
}

/* ---- mode preflight (urma_cli's, plus the cap for the size filter) ---- */

fn preflight(m: &ModeArgs) -> Result<(TransMode, TpType, bool, DeviceCap)> {
    let mode = m.mode.get();
    let tp = m.tp.get();
    let mut multi_path = m.multi_path || m.dev.starts_with("bonding");
    let cap = query_device(&m.dev)?;

    let mut missing = Vec::new();
    if !cap.supports_mode(mode) {
        missing.push(format!("transport mode {}", mode.name()));
    } else if !cap.supports(mode, tp) {
        missing.push(format!(
            "tp type {} for {}{}",
            tp.name(),
            mode.name(),
            if tp == TpType::Ctp && cap.tp_cap(mode).ctp && !cap.ctp_en {
                " (the mode allows CTP but the device feature ctp_en is off)"
            } else {
                ""
            }
        ));
    }
    if multi_path && !cap.supports_multi_path(mode) {
        if m.multi_path {
            missing.push(format!("multi-path for {}", mode.name()));
        } else {
            multi_path = false;
            println!(
                "[mode] note: {} does not report multi-path capability for {}; using single-path",
                m.dev,
                mode.name()
            );
        }
    }
    println!(
        "[mode] device {} mode {}-{}{}: {}",
        m.dev,
        mode.name(),
        tp.name(),
        if multi_path { " multi-path" } else { "" },
        cap
    );
    if !missing.is_empty() {
        return Err(Error::Invalid(format!(
            "device '{}' does not support it; missing: {}",
            m.dev,
            missing.join(", ")
        )));
    }
    Ok((mode, tp, multi_path, cap))
}

/* ============================== tcp: serve =============================== */

fn serve_tcp_run(a: &ServeTcpArgs) -> Result<()> {
    let buf_len = resolve_buf_len(a.buf_len, a.sizes.as_deref())?;
    let mut buf = vec![0u8; buf_len];
    fill_pat(&mut buf);
    let listener = TcpListener::bind((a.addr.as_str(), a.port))?;
    println!(
        "[serve-tcp] listening on {} ({buf_len}-byte pattern buffer, serves until killed)",
        listener.local_addr()?,
    );
    println!("[serve-tcp] one op = 4-byte LE length request -> that many bytes back");
    loop {
        let (mut stream, peer) = listener.accept()?;
        stream.set_nodelay(true)?;
        println!("[serve-tcp] connection from {peer}");
        let mut ops = 0u64;
        let mut bytes = 0u64;
        loop {
            let mut req = [0u8; 4];
            match stream.read_exact(&mut req) {
                Ok(()) => {}
                Err(e) if e.kind() == ErrorKind::UnexpectedEof => break, /* client closed */
                Err(e) => return Err(e.into()),
            }
            let n = u32::from_le_bytes(req) as usize;
            if n > buf.len() {
                return Err(Error::Invalid(format!(
                    "client asked for {n} bytes but the buffer holds {} (raise --buf-len on serve-tcp)",
                    buf.len()
                )));
            }
            stream.write_all(&buf[..n])?;
            ops += 1;
            bytes += n as u64;
        }
        println!("[serve-tcp] {peer} closed after {ops} ops / {bytes} bytes");
    }
}

/* ============================== tcp: read ================================ */

fn read_tcp_run(a: &ReadTcpArgs) -> Result<()> {
    let sizes = bench_sizes(&a.bench)?;
    let mut stream = connect_retry(&a.addr, a.port)?;
    stream.set_nodelay(true)?;
    println!(
        "[read-tcp] connected to {} (TCP_NODELAY on)",
        stream.peer_addr()?
    );

    let max_size = *sizes.last().unwrap();
    let mut rbuf = vec![0u8; max_size];
    print_header("tcp", a.bench.iters, a.bench.warmup, a.bench.csv);
    for &size in &sizes {
        let req = (size as u32).to_le_bytes();
        /* verify pass outside the timed loop, same policy as the URMA side */
        stream.write_all(&req)?;
        stream.read_exact(&mut rbuf[..size])?;
        check_pat(&format!("size {size} verify response"), &rbuf[..size])?;
        let s = run_iters(
            || {
                stream.write_all(&req)?;
                stream.read_exact(&mut rbuf[..size])?;
                Ok(())
            },
            a.bench.warmup,
            a.bench.iters,
        )?;
        print_row("tcp", size, a.bench.iters, &s, a.bench.csv);
    }
    println!("[read-tcp] done: {} sizes, {} iters each", sizes.len(), a.bench.iters);
    Ok(())
}

/// bounded connect retry, so the two sides can start in any order without
/// hanging forever on a dead peer
fn connect_retry(addr: &str, port: u16) -> Result<TcpStream> {
    let mut last = std::io::Error::from(ErrorKind::ConnectionRefused);
    for _ in 0..CONNECT_TRIES {
        match TcpStream::connect((addr, port)) {
            Ok(s) => return Ok(s),
            Err(e) => {
                last = e;
                std::thread::sleep(CONNECT_INTERVAL);
            }
        }
    }
    Err(Error::Io(last))
}

/* ============================== benchmark core =========================== */

/// parse --sizes and validate --iters; shared by both readers
fn bench_sizes(b: &BenchArgs) -> Result<Vec<usize>> {
    if b.iters == 0 {
        return Err(Error::Invalid("--iters must be at least 1".into()));
    }
    parse_sizes(&b.sizes)
}

fn parse_sizes(spec: &str) -> Result<Vec<usize>> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(Error::Invalid("--sizes is empty".into()));
    }
    /* "first..last" = doubling sweep, both ends inclusive: 8..16m gives
       8,16,...,16M (the last step is the largest doubling that still fits);
       anything else is a comma list */
    if let Some((first, last)) = spec.split_once("..") {
        let first = parse_one_size(first)?;
        let last = parse_one_size(last)?;
        if last < first {
            return Err(Error::Invalid(format!("empty size range {first}..{last}")));
        }
        let mut sizes = vec![first];
        while let Some(next) = sizes.last().unwrap().checked_mul(2) {
            if next > last {
                break;
            }
            sizes.push(next);
        }
        return Ok(sizes);
    }
    let mut sizes = Vec::new();
    for part in spec.split(',') {
        if part.trim().is_empty() {
            continue;
        }
        sizes.push(parse_one_size(part)?);
    }
    if sizes.is_empty() {
        return Err(Error::Invalid("--sizes is empty".into()));
    }
    sizes.sort_unstable();
    sizes.dedup();
    Ok(sizes)
}

/// one size token: plain bytes ("4096") or with a binary suffix ("4k"/"1m"/"2g",
/// case-insensitive); 0 and anything above the u32 sge length limit rejected
fn parse_one_size(tok: &str) -> Result<usize> {
    let tok = tok.trim();
    let (num, mult) = match tok.as_bytes().last() {
        Some(b'k' | b'K') => (&tok[..tok.len() - 1], 1 << 10),
        Some(b'm' | b'M') => (&tok[..tok.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&tok[..tok.len() - 1], 1 << 30),
        _ => (tok, 1),
    };
    let n: usize =
        num.parse().map_err(|_| Error::Invalid(format!("bad size '{tok}' in --sizes")))?;
    let n = n.checked_mul(mult).ok_or_else(|| Error::Invalid(format!("size '{tok}' overflows")))?;
    if n == 0 {
        return Err(Error::Invalid("--sizes contains 0".into()));
    }
    if n > u32::MAX as usize {
        return Err(Error::Invalid(format!("size {n} exceeds the u32 sge length limit")));
    }
    Ok(n)
}

/// serve-side buffer size: explicit --buf-len wins, else the max of --sizes
/// (when the sweep is declared), else DEFAULT_BUF_LEN
fn resolve_buf_len(buf_len: Option<usize>, sizes: Option<&str>) -> Result<usize> {
    if let Some(n) = buf_len {
        if n == 0 {
            return Err(Error::Invalid("--buf-len must be at least 1".into()));
        }
        return Ok(n);
    }
    if let Some(spec) = sizes {
        return Ok(*parse_sizes(spec)?.last().unwrap());
    }
    Ok(DEFAULT_BUF_LEN)
}

/// run `op` warmup+iters times, timing only the last `iters` calls
fn run_iters(mut op: impl FnMut() -> Result<()>, warmup: u32, iters: u32) -> Result<Stats> {
    for _ in 0..warmup {
        op()?;
    }
    let mut samples = Vec::with_capacity(iters as usize);
    for _ in 0..iters {
        let t0 = Instant::now();
        op()?;
        samples.push(t0.elapsed());
    }
    Ok(stats(&samples))
}

/// one report row, in µs. Percentiles are nearest-rank on the sorted samples
/// (index ceil(p%·n)−1): "at least p% of the samples are ≤ this".
struct Stats {
    min: f64,
    p50: f64,
    avg: f64,
    p99: f64,
    max: f64,
}

fn stats(samples: &[Duration]) -> Stats {
    debug_assert!(!samples.is_empty());
    let mut ns: Vec<u128> = samples.iter().map(|d| d.as_nanos()).collect();
    ns.sort_unstable();
    let pct = |p: f64| -> f64 {
        let n = ns.len();
        let idx = ((p / 100.0 * n as f64).ceil() as usize).saturating_sub(1).min(n - 1);
        ns[idx] as f64 / 1000.0
    };
    let total: u128 = ns.iter().sum();
    Stats {
        min: ns[0] as f64 / 1000.0,
        p50: pct(50.0),
        avg: total as f64 / ns.len() as f64 / 1000.0,
        p99: pct(99.0),
        max: *ns.last().unwrap() as f64 / 1000.0,
    }
}

fn print_header(transport: &str, iters: u32, warmup: u32, csv: bool) {
    if csv {
        println!("csv,transport,size_bytes,iters,min_us,p50_us,avg_us,p99_us,max_us");
    } else {
        println!("== {transport} READ latency: {iters} iters + {warmup} warmup per size, µs ==");
        println!("{:>10} {:>10} {:>10} {:>10} {:>10} {:>10}", "size", "min", "p50", "avg", "p99", "max");
    }
}

fn print_row(transport: &str, size: usize, iters: u32, s: &Stats, csv: bool) {
    if csv {
        println!(
            "csv,{transport},{size},{iters},{:.2},{:.2},{:.2},{:.2},{:.2}",
            s.min, s.p50, s.avg, s.p99, s.max
        );
    } else {
        println!("{:>10} {:>10.2} {:>10.2} {:>10.2} {:>10.2} {:>10.2}", size, s.min, s.p50, s.avg, s.p99, s.max);
    }
}

/* ---- data pattern: position-dependent, so a wrong offset/length or a
   short/corrupted transfer is caught by one comparison ---- */

fn pat(i: usize) -> u8 {
    (i as u8).wrapping_mul(31).wrapping_add(7)
}

fn fill_pat(buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate() {
        *b = pat(i);
    }
}

fn check_pat(what: &str, buf: &[u8]) -> Result<()> {
    for (i, &b) in buf.iter().enumerate() {
        if b != pat(i) {
            return Err(Error::Invalid(format!(
                "{what}: byte {i} is {b:#x}, expected {:#x} — wrong offset/length or corrupted transfer",
                pat(i)
            )));
        }
    }
    Ok(())
}

/* ============================== wire descriptor ========================== */
/* hand-rolled hex, the same layout as urma_cli's (this example is pure std
   too, so common/mod.rs — with its serde/tokio — is deliberately not used) */

/// Everything read-urma needs on the other node: the plain [`SegDesc`] (the
/// length ceiling, the eid for the loopback guard) plus the two import blobs.
struct WireDesc {
    seg: SegDesc,
    seg_ctx: Vec<u8>,
    rjetty: Vec<u8>,
}

/// little-endian: eid[16] uasid va len attr token_id seg_len rjetty_len blobs
fn pack_desc(d: &WireDesc) -> String {
    let seg = &d.seg;
    let mut b = Vec::with_capacity(52 + d.seg_ctx.len() + d.rjetty.len());
    b.extend_from_slice(&seg.eid.0);
    b.extend_from_slice(&seg.uasid.to_le_bytes());
    b.extend_from_slice(&seg.va.to_le_bytes());
    b.extend_from_slice(&seg.len.to_le_bytes());
    b.extend_from_slice(&seg.attr.to_le_bytes());
    b.extend_from_slice(&seg.token_id.to_le_bytes());
    b.extend_from_slice(&(d.seg_ctx.len() as u32).to_le_bytes());
    b.extend_from_slice(&(d.rjetty.len() as u32).to_le_bytes());
    b.extend_from_slice(&d.seg_ctx);
    b.extend_from_slice(&d.rjetty);
    hex_enc(&b)
}

fn unpack_desc(s: &str) -> Result<WireDesc> {
    let bytes = hex_dec(s)?;
    let mut rd = Rd { b: &bytes };
    let mut eid = [0u8; 16];
    eid.copy_from_slice(rd.take(16, "eid")?);
    let seg = SegDesc {
        eid: Eid(eid),
        uasid: rd.u32("uasid")?,
        va: rd.u64("va")?,
        len: rd.u64("len")?,
        attr: rd.u32("attr")?,
        token_id: rd.u32("token_id")?,
    };
    let seg_len = rd.u32("seg ctx length")? as usize;
    let rj_len = rd.u32("rjetty length")? as usize;
    let seg_ctx = rd.take(seg_len, "seg ctx")?.to_vec();
    let rjetty = rd.take(rj_len, "rjetty")?.to_vec();
    if !rd.b.is_empty() {
        return Err(Error::Invalid(format!("{} trailing bytes in descriptor", rd.b.len())));
    }
    Ok(WireDesc { seg, seg_ctx, rjetty })
}

/// little reader over the decoded hex bytes
struct Rd<'a> {
    b: &'a [u8],
}

impl<'a> Rd<'a> {
    fn take(&mut self, n: usize, what: &str) -> Result<&'a [u8]> {
        if n > self.b.len() {
            return Err(Error::Invalid(format!("descriptor truncated at {what}")));
        }
        let (head, tail) = self.b.split_at(n);
        self.b = tail;
        Ok(head)
    }

    fn u32(&mut self, what: &str) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4, what)?.try_into().unwrap()))
    }

    fn u64(&mut self, what: &str) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8, what)?.try_into().unwrap()))
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex_enc(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

fn hex_dec(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if !s.len().is_multiple_of(2) {
        return Err(Error::Invalid("odd-length hex descriptor".into()));
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.as_bytes().chunks_exact(2) {
        out.push(hex_val(pair[0])? << 4 | hex_val(pair[1])?);
    }
    Ok(out)
}

fn hex_val(c: u8) -> Result<u8> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(Error::Invalid(format!("bad hex digit '{}'", c as char))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desc_hex_roundtrip() {
        let d = WireDesc {
            seg: SegDesc {
                eid: Eid([0x11; 16]),
                uasid: 0x2233,
                va: 0x1234_5678,
                len: 4096,
                attr: 0x4001,
                token_id: 7,
            },
            seg_ctx: vec![0xaa; 48],
            rjetty: vec![0xbb; 55],
        };
        let hex = pack_desc(&d);
        let rt = unpack_desc(&hex).expect("roundtrip");
        assert_eq!(
            (rt.seg.eid, rt.seg.uasid, rt.seg.va, rt.seg.len, rt.seg.attr, rt.seg.token_id),
            (d.seg.eid, d.seg.uasid, d.seg.va, d.seg.len, d.seg.attr, d.seg.token_id)
        );
        assert!(unpack_desc("abc").is_err()); /* odd length */
        assert!(unpack_desc(&hex[..20]).is_err()); /* truncated */
        assert!(unpack_desc(&format!("{hex}00")).is_err()); /* trailing bytes */
        assert_eq!(rt.seg_ctx, d.seg_ctx);
        assert_eq!(rt.rjetty, d.rjetty);
    }

    #[test]
    fn sizes_parse_dedup_sort() {
        assert_eq!(parse_sizes("1024,8,64,8, 256").unwrap(), vec![8, 64, 256, 1024]);
        assert_eq!(parse_sizes("8, ,64").unwrap(), vec![8, 64]); /* blanks tolerated */
        assert_eq!(parse_sizes("4096").unwrap(), vec![4096]);
        assert!(parse_sizes("").is_err());
        assert!(parse_sizes("8,x").is_err());
        assert!(parse_sizes("0").is_err());
        assert!(parse_sizes("8,4294967296").is_err()); /* above the u32 sge limit */
    }

    #[test]
    fn sizes_range_and_suffixes() {
        assert_eq!(parse_sizes("8..32").unwrap(), vec![8, 16, 32]);
        assert_eq!(parse_sizes("4k..16K").unwrap(), vec![4096, 8192, 16384]);
        assert_eq!(parse_sizes("8..1000").unwrap(), vec![8, 16, 32, 64, 128, 256, 512]);
        let m = parse_sizes("8..16m").unwrap();
        assert_eq!((*m.first().unwrap(), *m.last().unwrap()), (8, 16 << 20));
        assert!(m.iter().zip(m.iter().skip(1)).all(|(a, b)| b == &(a * 2)));
        assert_eq!(parse_sizes("1k,2K,1m,1M").unwrap(), vec![1024, 2048, 1048576]);
        assert!(parse_sizes("8..4").is_err()); /* empty range */
        assert!(parse_sizes("0..8").is_err());
        assert!(parse_sizes("8..").is_err());
        assert!(parse_sizes("1x").is_err());
        assert!(parse_sizes("8..16,32").is_err()); /* a range is never a list */
    }

    #[test]
    fn serve_buf_len_resolution() {
        assert_eq!(resolve_buf_len(None, None).unwrap(), DEFAULT_BUF_LEN);
        assert_eq!(resolve_buf_len(Some(123), Some("8..16m")).unwrap(), 123); /* explicit wins */
        assert_eq!(resolve_buf_len(None, Some("8..16m")).unwrap(), 16 << 20);
        assert_eq!(resolve_buf_len(None, Some("1k,2m")).unwrap(), 2 << 20);
        assert!(resolve_buf_len(Some(0), None).is_err());
        assert!(resolve_buf_len(None, Some("junk")).is_err());
    }

    #[test]
    fn stats_nearest_rank() {
        let samples: Vec<Duration> = (1..=100u64).map(Duration::from_nanos).collect();
        let s = stats(&samples);
        assert!((s.min - 0.001).abs() < 1e-9);
        assert!((s.p50 - 0.050).abs() < 1e-9); /* ceil(0.5·100)−1 = 49 → 50ns */
        assert!((s.p99 - 0.099).abs() < 1e-9); /* ceil(0.99·100)−1 = 98 → 99ns */
        assert!((s.max - 0.100).abs() < 1e-9);
        assert!((s.avg - 0.0505).abs() < 1e-9);
        let one = stats(&[Duration::from_micros(7)]);
        assert!((one.min - 7.0).abs() < 1e-9 && (one.p99 - 7.0).abs() < 1e-9);
    }
}
