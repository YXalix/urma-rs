//! READ-semantics latency/bandwidth benchmark: URMA one-sided READ vs the
//! closest TCP equivalent, swept over message sizes.
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
//! (post → completion / request → full response; the URMA-only `--depth`
//! below relaxes this on purpose). TCP_NODELAY is forced on both ends
//! (Nagle + delayed ACK would add ~40ms artifacts).
//!
//! `read-urma --depth N` (default 1) switches the URMA side from the
//! serialized latency measurement to a pipelined bandwidth one, in
//! urma_perftest read_bw's style: up to N READs in flight, post-one /
//! reap-one. Both transfer ends rotate through their disjoint windows per
//! op — the landing buffer is registered at size×depth when memory allows
//! (capped at 1 GiB, with a note when the cap bites), and the remote side
//! cycles the peer's segment, so start serve-urma with a `--buf-len` above
//! the sweep max to widen the remote rotation too; hammering one address
//! range saturates that memory region, not the link. Reported per size
//! instead of the latency percentiles: average BW over the whole
//! first-post→last-completion window and Mops. There is deliberately no
//! peak column: in a full pipeline every per-op post→completion window
//! contains queueing time, so a "fastest op" is pipeline noise, not a
//! fabric property. `--duration S` runs each size to a seconds-long
//! deadline instead of `--iters` ops (warmup becomes a fixed 1s) — with
//! iteration counts, a big-size row's window shrinks to tens of ms and one
//! scheduler hiccup dominates the average. TCP stays at depth 1 by
//! construction (a request/response pair cannot pipeline), which is itself
//! the gap.
//!
//! serve-urma/read-urma follow urma_cli's pure flow (the descriptor travels
//! as one hand-packed hex line, imports use the export blobs; defaults are
//! CTP-RM like every example):
//!
//! ```bash
//! # URMA READ latency between two nodes:
//! nodeA$ cargo run --example read_bench -- serve-urma -d bonding_dev_0 --sizes 8..16m
//! nodeB$ cargo run --example read_bench -- read-urma -d bonding_dev_0 '<[desc] hex>' \
//!         --sizes 8..16m   # serve sizes its buffer to the sweep's maximum
//!
//! # URMA READ bandwidth: same sweep, pipelined depth (perftest read_bw style):
//! nodeA$ cargo run --example read_bench -- serve-urma -d bonding_dev_0 --sizes 4k..1m --buf-len 64m
//! nodeB$ cargo run --example read_bench -- read-urma -d bonding_dev_0 '<[desc] hex>' \
//!         --sizes 4k..1m --depth 32 --duration 10
//!
//! # TCP reference over the same pair of machines:
//! nodeA$ cargo run --example read_bench -- serve-tcp
//! nodeB$ cargo run --example read_bench -- read-tcp --addr <ipA>
//! ```
//!
//! `scripts/test_readbench.sh` automates the matrix: TCP loopback as a local
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
/// warmup length in --duration mode (bandwidth): long enough to reach steady
/// state at every size, unlike a fixed op count whose duration shrinks with
/// the size
const BW_WARMUP: Duration = Duration::from_secs(1);
/// landing-buffer ceiling for bandwidth mode (size x depth, capped): keeps a
/// 16m x 64-style sweep from demanding GiBs of registered memory
const BW_LANDING_CAP: usize = 1 << 30;

#[derive(Parser)]
#[command(
    name = "read_bench",
    version,
    about = "READ-semantics latency/bandwidth benchmark: URMA one-sided READ vs TCP \
             request/response, swept over message sizes (min/p50/avg/p99/max per size; \
             read-urma --depth N>1 pipelines N outstanding READs and reports avg MiB/s + \
             Mops instead; --duration switches to seconds-long windows)",
    after_help = "READ semantics comparison: a URMA READ is one-sided (the server CPU sleeps); \
                  TCP has no one-sided op, so a read is emulated as a 4-byte length request + \
                  N-byte response round trip. Both transports share the sweep, the verify pass \
                  and the warmup. read-urma --depth>1 switches the URMA side to a pipelined \
                  bandwidth measurement (urma_perftest read_bw's measurement, both transfer \
                  ends rotating their windows; --duration for stable long windows). \
                  scripts/test_readbench.sh runs the full matrix."
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
    /// print csv rows instead of the table (schema in the header line:
    /// latency percentiles, or ops/depth/bw_avg_mib/mops in bandwidth mode)
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
    /// outstanding READs: 1 = latency mode (serialized, default); >1 = bandwidth
    /// mode - up to this many READs in flight, reported as avg MiB/s + Mops
    /// instead of latency percentiles (bounded by the jetty/CQ depth, 64)
    #[arg(long, default_value_t = 1)]
    depth: u32,
    /// timed window per size in seconds, bandwidth mode only (0 = off): each
    /// size runs to this deadline instead of --iters ops, then the in-flight
    /// READs drain - a window long enough that one scheduler hiccup cannot
    /// dominate the average; --warmup becomes a fixed 1s in this mode
    #[arg(long, default_value_t = 0)]
    duration: u64,
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
    if a.depth == 0 || a.depth > DEFAULT_DEPTH {
        return Err(Error::Invalid(format!(
            "--depth must be 1..={DEFAULT_DEPTH}: the jetty/CQ are created at DEFAULT_DEPTH, \
             more outstanding READs than that would overflow the queues"
        )));
    }
    if a.duration > 0 && a.depth == 1 {
        return Err(Error::Invalid(
            "--duration needs bandwidth mode: pass --depth > 1 (latency mode counts iterations, \
             not seconds)"
                .into(),
        ));
    }
    let duration = (a.duration > 0).then_some(a.duration);

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
    /* bandwidth mode registers size x depth of landing (capped) so the
       pipeline gets disjoint landing windows; the remote end rotates the
       peer's segment — give serve-urma a --buf-len above the sweep max to
       widen that rotation too */
    let landing_len = if a.depth > 1 {
        let want = max_size.saturating_mul(a.depth as usize);
        let capped = want.min(BW_LANDING_CAP);
        if capped < want {
            println!(
                "[read-urma] note: landing capped at {capped} bytes ({max_size} x depth {} would be {want}); the top sizes overlap windows",
                a.depth
            );
        }
        println!(
            "[read-urma] bandwidth mode: depth {}, landing {capped} bytes, remote rotation within the {}-byte peer segment",
            a.depth,
            wire.seg.len
        );
        capped
    } else {
        max_size
    };
    println!(
        "[4/4] register landing buffer ({landing_len} bytes), sweep {} sizes: {}",
        sizes.len(),
        sizes.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(",")
    );
    let landing = RegisteredBuf::new(&ctx, landing_len, TOKEN_VALUE)?;
    let remote_va = wire.seg.va;

    print_header("urma", a.bench.iters, a.bench.warmup, a.depth, a.duration, a.bench.csv);
    for &size in &sizes {
        let sge = landing.sge(0, size as u32)?;
        /* verify pass outside the timed loop: one serialized READ, compare the
           pattern — kept at depth 1 even in bandwidth mode (a correctness pass
           must not overlap in-flight READs) */
        jetty.post_read(&peer, remote_va, &[sge], READ_CTX)?;
        let _ = wait_read_spin(&cq)?;
        check_pat(&format!("size {size} verify read"), &landing[..size])?;
        if a.depth == 1 {
            let s = run_iters(
                || {
                    jetty.post_read(&peer, remote_va, &[sge], READ_CTX)?;
                    wait_read_spin(&cq).map(|_| ())
                },
                a.bench.warmup,
                a.bench.iters,
            )?;
            print_row("urma", size, a.bench.iters, &s, a.bench.csv);
        } else {
            let bw = BwCtx {
                jetty: &jetty,
                cq: &cq,
                peer: &peer,
                remote_va,
                seg_len: wire.seg.len,
                landing: &landing,
            };
            let s = run_bw_iters(&bw, size, a.depth, a.bench.warmup, a.bench.iters, duration)?;
            print_bw_row("urma", size, a.depth, &s, a.bench.csv);
        }
    }
    let per = if a.depth > 1 && a.duration > 0 {
        format!("{}s window", a.duration)
    } else {
        format!("{} iters", a.bench.iters)
    };
    println!("[read-urma] done: {} sizes, {per} each", sizes.len());
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

/* =========================== urma: read (bandwidth) ====================== */

/// the fixed post parameters shared by the pipelined bandwidth passes
struct BwCtx<'a> {
    jetty: &'a Jetty,
    cq: &'a CompletionQueue,
    peer: &'a Peer,
    remote_va: u64,
    /// peer segment length: the remote rotation window
    seg_len: u64,
    landing: &'a RegisteredBuf,
}

/// landing offset for op `i` of the bandwidth loop: the `len/size` disjoint
/// windows of the buffer, cycled per op (perftest's cycle buffer, applied to
/// BOTH transfer ends — same source bytes every op is what saturates a
/// single memory region instead of the link). With one window (size = whole
/// buffer) the in-flight READs overlap — fine: identical source bytes, and
/// the timed loop never checks data (the serialized verify pass does),
/// matching perftest's practice.
fn bw_slot_off(i: u64, size: usize, len: usize) -> usize {
    let slots = (len / size).max(1) as u64;
    ((i % slots) as usize) * size
}

/// when a bandwidth pass stops posting: after `ops` posts, or at a deadline.
/// The Until arm always allows at least one post, so a timed pass always has
/// a window to report.
enum BwStop {
    Ops(u64),
    Until(Instant),
}

impl BwStop {
    fn stop_posting(&self, next: u64) -> bool {
        match self {
            BwStop::Ops(n) => next >= *n,
            BwStop::Until(t) => next > 0 && Instant::now() >= *t,
        }
    }
}

/// one pipelined pass: post while the pipeline is not full and the stop
/// condition allows, reap completions as they land (each one frees a slot
/// for the next post). A timed pass returns its whole
/// first-post→last-completion window plus the completed-op count.
/// Completions only need counting — no per-op timestamps: in a full
/// pipeline every post→completion window contains queueing time, so per-op
/// figures are pipeline noise, not fabric properties. The hang guard resets
/// on every completion: it bounds silence, not the pass (a duration pass
/// runs long by design).
fn bw_pass(
    bw: &BwCtx, size: usize, depth: u32, stop: &BwStop, timed: bool,
) -> Result<Option<(Duration, u64)>> {
    let (mut next, mut done) = (0u64, 0u64);
    let mut t0 = None;
    let mut deadline = Instant::now() + READ_TIMEOUT;
    loop {
        while !stop.stop_posting(next) && next - done < u64::from(depth) {
            let off = bw_slot_off(next, size, bw.landing.len());
            let va = bw.remote_va + bw_slot_off(next, size, bw.seg_len as usize) as u64;
            bw.jetty.post_read(bw.peer, va, &[bw.landing.sge(off, size as u32)?], READ_CTX)?;
            if timed && next == 0 {
                t0 = Some(Instant::now());
            }
            next += 1;
        }
        if done == next && stop.stop_posting(next) {
            break;
        }
        match bw.cq.poll()? {
            Some(cr) => {
                if !cr.is_success() {
                    return Err(Error::BadCompletion { status: cr.status, user_ctx: cr.user_ctx });
                }
                done += 1;
                deadline = Instant::now() + READ_TIMEOUT;
            }
            None => {
                if Instant::now() >= deadline {
                    return Err(Error::PollTimeout { user_ctx: next });
                }
                std::hint::spin_loop();
            }
        }
    }
    Ok(t0.map(|t0| (t0.elapsed(), done)))
}

/// bandwidth counterpart of `run_iters`: an untimed warmup pass, then the
/// timed one. Op-count mode uses --warmup/--iters; duration mode warms up a
/// fixed 1s and then runs to a seconds-long deadline — a window long enough
/// that one scheduler hiccup cannot dominate the average.
fn run_bw_iters(
    bw: &BwCtx, size: usize, depth: u32, warmup: u32, iters: u32, duration: Option<u64>,
) -> Result<BwStats> {
    let (warm_stop, main_stop) = match duration {
        Some(secs) => (
            BwStop::Until(Instant::now() + BW_WARMUP),
            BwStop::Until(Instant::now() + Duration::from_secs(secs)),
        ),
        None => (BwStop::Ops(u64::from(warmup)), BwStop::Ops(u64::from(iters))),
    };
    if matches!(warm_stop, BwStop::Ops(n) if n > 0) {
        bw_pass(bw, size, depth, &warm_stop, false)?;
    }
    let (window, ops) =
        bw_pass(bw, size, depth, &main_stop, true)?.expect("timed pass posts at least one op");
    Ok(bw_stats(size, ops, window))
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
    print_header("tcp", a.bench.iters, a.bench.warmup, 1, 0, a.bench.csv);
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

/// one bandwidth report row (read-urma --depth>1): avg in MiB/s (binary,
/// matching the k/m size suffixes) and ops per second in millions, both over
/// the whole first-post→last-completion window — the numbers a streaming
/// consumer sees. No peak: in a full pipeline every per-op post→completion
/// window contains queueing time, so a "fastest op" is pipeline noise, not
/// a fabric property (perftest's BW peak has the same artifact).
struct BwStats {
    ops: u64,
    avg: f64,
    mops: f64,
}

/// pure math behind [`BwStats`], split out for the unit test
fn bw_stats(size: usize, ops: u64, total: Duration) -> BwStats {
    let secs = total.as_secs_f64();
    BwStats {
        ops,
        avg: size as f64 * ops as f64 / secs / (1024.0 * 1024.0),
        mops: ops as f64 / secs / 1e6,
    }
}

fn print_header(transport: &str, iters: u32, warmup: u32, depth: u32, duration: u64, csv: bool) {
    if csv {
        if depth > 1 {
            println!("csv,transport,size_bytes,ops,depth,bw_avg_mib,mops");
        } else {
            println!("csv,transport,size_bytes,iters,min_us,p50_us,avg_us,p99_us,max_us");
        }
    } else if depth > 1 {
        let mode = if duration > 0 {
            format!("{duration}s window + 1s warmup")
        } else {
            format!("{iters} iters + {warmup} warmup")
        };
        println!("== {transport} READ bandwidth: depth {depth}, {mode} per size, MiB/s ==");
        println!("{:>10} {:>10} {:>10}", "size", "avg", "Mops");
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

fn print_bw_row(transport: &str, size: usize, depth: u32, s: &BwStats, csv: bool) {
    if csv {
        println!("csv,{transport},{size},{},{depth},{:.2},{:.3}", s.ops, s.avg, s.mops);
    } else {
        println!("{:>10} {:>10.2} {:>10.3}", size, s.avg, s.mops);
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

    #[test]
    fn bw_stats_math() {
        /* 1000x4096B over a 10ms window = 409.6MB/s = 390.625 MiB/s; 0.1 Mops */
        let s = bw_stats(4096, 1000, Duration::from_millis(10));
        assert_eq!(s.ops, 1000);
        assert!((s.avg - 390.625).abs() < 1e-6);
        assert!((s.mops - 0.1).abs() < 1e-9);
        /* duration mode counts ops itself; avg stays bytes over the window */
        let s = bw_stats(65536, 3_000_000, Duration::from_secs(10));
        assert!((s.avg - 65536.0 * 300_000.0 / (1024.0 * 1024.0)).abs() < 1e-6);
        assert!((s.mops - 0.3).abs() < 1e-9);
    }

    #[test]
    fn bw_slot_cycle() {
        /* four disjoint 1024B windows in a 4096B landing buffer, cycled per op */
        let offs: Vec<usize> = (0..6).map(|i| bw_slot_off(i, 1024, 4096)).collect();
        assert_eq!(offs, vec![0, 1024, 2048, 3072, 0, 1024]);
        /* size = whole buffer: single window, always offset 0 (overlap accepted) */
        assert_eq!(bw_slot_off(0, 4096, 4096), 0);
        assert_eq!(bw_slot_off(9, 4096, 4096), 0);
        /* size not dividing the buffer: floor windows, never out of bounds */
        assert_eq!(bw_slot_off(3, 3000, 4096), 0);
        for i in 0..100u64 {
            let off = bw_slot_off(i, 1000, 4096);
            assert!(off + 1000 <= 4096);
        }
    }
}
