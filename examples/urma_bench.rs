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
//! urma_perftest read_bw's style: up to N READs in flight, post-burst /
//! batch-reap. Saturating a link needs `depth × size` above the fabric's
//! bandwidth-latency product — 64 outstanding 4K READs are 256 KiB in
//! flight, well under a 400G-class fabric's BDP, so `--depth` is bounded
//! only by the device's per-queue ceilings (jfs/jfc/jfr depth caps from
//! `query_device`; the queues are created at the requested depth) and small
//! sizes need depths in the hundreds to fill the pipe. Two more perftest
//! mechanics keep the reader's own CPU from becoming the bottleneck at
//! small sizes, where line rate means millions of ops/s:
//! completions are reaped `urma_poll_jfc`-batched (one CQ lock + software
//! doorbell per batch, not per record), and `--cq-mod m` posts only every
//! m-th READ with a completion record (CQ moderation; default 0 = auto:
//! min(100, depth) at every size — per-op completion processing is exactly
//! what caps the ops rate of a full pipeline, from a 4K sweep to a
//! ~0.5-Mops 64 KiB one; keep depth well above the moderation, since
//! done-counting advances in m-sized jumps and the in-flight window
//! bottoms out near depth − m). Signaled READs
//! carry comp_order, so each record proves every earlier op completed, and
//! a window ending on an unsignaled tail is closed by one extra 1-byte
//! fence READ whose record is not counted; op accounting is therefore
//! exact at any moderation.
//! Past the completion path, two more perftest levers attack the two
//! remaining rate limiters: `--post-list L` chains L READs into ONE post
//! call (`urma_jfs_wr_t.next`, one queue-lock + doorbell per list) — a
//! single-threaded poster otherwise tops out near 2 Mops — and
//! `--jetties N` spreads the sweep over N parallel jettys (serve-urma
//! `--jetties N` exports one rjetty blob per jetty; each local jetty
//! pairs with one, all completions land on the shared CQ), the probe for
//! a per-jetty fabric IOPS ceiling. Ops are assigned chunk-round-robin so
//! both knobs compose, and moderation/fence bookkeeping runs per jetty
//! stream, keeping the accounting exact at any combination.
//! Both transfer ends rotate through their windows per op — the landing
//! buffer is registered at size×depth (`--landing-cap` ceiling, default
//! 1 GiB, with an overlap note when it bites), and the remote side cycles
//! the peer's segment, so start serve-urma with a `--buf-len` above
//! sweep-max × depth to widen that rotation too (a note fires when the
//! remote windows are fewer than the depth); hammering one address range
//! saturates that memory region, not the link. Reported per size instead
//! of the latency percentiles: average BW over the whole
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
//! CTP-RM like every example). `write-urma` runs the same sweep for the
//! other one-sided direction (perftest write_bw): the landing buffer
//! becomes the local SOURCE and the peer's segment the destination, verify
//! flips into one serialized WRITE + READ-back, and every bandwidth knob
//! (depth/jetties/post-list/cq-mod/landing-cap/duration) carries over —
//! the READ-vs-WRITE sustained-bandwidth asymmetry is then a same-tool,
//! same-conditions comparison. WRITEs overwrite the serve's pattern, so
//! restart serve-urma before a read-urma verify against the same segment.
//!
//! ```bash
//! # URMA READ latency between two nodes:
//! nodeA$ cargo run --example urma_bench -- serve-urma -d bonding_dev_0 --sizes 8..16m
//! nodeB$ cargo run --example urma_bench -- read-urma -d bonding_dev_0 '<[desc] hex>' \
//!         --sizes 8..16m   # serve sizes its buffer to the sweep's maximum
//!
//! # URMA READ bandwidth: same sweep, pipelined depth (perftest read_bw style).
//! # Small sizes need a deep pipeline + CQ moderation to reach line rate; if
//! # 4K still plateaus around 2 Mops, chain posts (--post-list) and only then
//! # add parallel jettys (--jetties on BOTH sides) to tell a poster-CPU
//! # ceiling from a per-jetty fabric IOPS one:
//! nodeA$ cargo run --example urma_bench -- serve-urma -d bonding_dev_0 --sizes 4k..1m \
//!         --buf-len 512m --jetties 4
//! nodeB$ cargo run --example urma_bench -- read-urma -d bonding_dev_0 '<[desc] hex>' \
//!         --sizes 4k..1m --depth 512 --post-list 32 --jetties 4 --duration 10
//!
//! # TCP reference over the same pair of machines:
//! nodeA$ cargo run --example urma_bench -- serve-tcp
//! nodeB$ cargo run --example urma_bench -- read-tcp --addr <ipA>
//!
//! # URMA WRITE bandwidth (perftest write_bw, same knobs): the asymmetry
//! # probe next to the READ number above.
//! nodeA$ cargo run --example urma_bench -- serve-urma -d bonding_dev_0 --sizes 4k..1m \
//! #        --buf-len 512m --jetties 4
//! nodeB$ cargo run --example urma_bench -- write-urma -d bonding_dev_0 '<[desc] hex>' \
//! #        --sizes 4k..1m --depth 512 --post-list 32 --jetties 4 --duration 10
//! ```
//!
//! `--csv` prints `csv,transport,size_bytes,...` rows for plotting instead
//! of the table.

use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::{Args, Parser, Subcommand, ValueEnum};
use urma_rs::{
    query_device, Completion, CompletionQueue, Context, DeviceCap, Eid, Error, Jetty, JettyOpts,
    Peer, ReadReq, RegisteredBuf, Result, SegDesc, TpType, TransMode, Urma, WriteReq,
    DEFAULT_DEPTH, TOKEN_VALUE, URMA_MAX_PRIORITY,
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
/// (override with --landing-cap on machines with memory to spare)
const BW_LANDING_CAP: usize = 1 << 30;
/// completions reaped per urma_poll_jfc call in bandwidth mode: one call
/// takes the provider's CQ lock and writes its software doorbell once, so
/// batches amortize both (perftest's PERFTEST_POLL_BATCH is 16)
const BW_POLL_BATCH: usize = 16;
/// auto --cq-mod group size (perftest's PERFTEST_DEF_CQ_NUM): applied at
/// every size — per-op completion processing caps the ops rate of a full
/// pipeline at 4K and at 64 KiB alike
const BW_CQ_MOD_AUTO: u64 = 100;
/// jetty ceiling for bandwidth mode: the fence user_ctx space below encodes
/// the jetty index in the last BW_MAX_JETTIES values of u64
const BW_MAX_JETTIES: u32 = 64;
/// user_ctx base of the bandwidth fence READs: jetty j's fence posts with
/// `BW_FENCE_CTX_BASE - j` (never a real op index — those are counts, tiny);
/// each jetty's fence record closes that jetty's unsignaled tail
const BW_FENCE_CTX_BASE: u64 = u64::MAX;
/// lowest fence user_ctx (that of jetty BW_MAX_JETTIES-1): any ctx at or
/// above it decodes as a fence, anything below as an op index
const BW_FENCE_CTX_MIN: u64 = BW_FENCE_CTX_BASE - (BW_MAX_JETTIES - 1) as u64;

#[derive(Parser)]
#[command(
    name = "urma_bench",
    version,
    about = "One-sided READ/WRITE latency/bandwidth benchmark: URMA vs the TCP \
             request/response emulation, swept over message sizes (min/p50/avg/p99/max per \
             size; read-urma/write-urma --depth N>1 pipeline N outstanding ops and report \
             avg MiB/s + Mops instead; --duration switches to seconds-long windows)",
    after_help = "READ semantics comparison: a URMA READ is one-sided (the server CPU sleeps); \
                  TCP has no one-sided op, so a read is emulated as a 4-byte length request + \
                  N-byte response round trip. Both transports share the sweep, the verify pass \
                  and the warmup. read-urma/write-urma --depth>1 switches the URMA side to a \
                  pipelined bandwidth measurement (urma_perftest read_bw/write_bw's \
                  measurement: both transfer ends rotating their windows, batched completion \
                  reaping, --cq-mod completion moderation, depth bounded by the device's queue \
                  caps, --post-list chained posts and --jetties parallel streams for the \
                  ops-rate ceiling; --duration for stable long windows)."
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
    ReadUrma(OneSidedArgs),
    /// URMA client: timed one-sided WRITEs per size (perftest write_bw's
    /// measurement, same knobs as read-urma): the landing buffer becomes the
    /// local SOURCE, the peer's segment the destination; verification flips
    /// direction — one serialized WRITE of the pattern, then a READ-back.
    /// NOTE: WRITEs overwrite the serve's pattern, so restart serve-urma
    /// before a read-urma verify pass against the same segment.
    WriteUrma(OneSidedArgs),
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
    /// jfs priority slot 0..=15 (perftest's -O): the device's service class
    /// for the stream. Default: the device's slot for the selected --tp from
    /// its priority table (what urma_perftest auto-picks when -O is omitted),
    /// falling back to the wrapper's 15 default when the table has no such
    /// slot.
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=15))]
    priority: Option<u8>,
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
    /// latency percentiles, or ops/depth/cq_mod/bw_avg_mib/mops in
    /// bandwidth mode)
    #[arg(long)]
    csv: bool,
}

#[derive(Args)]
struct ServeUrmaArgs {
    #[command(flatten)]
    mode: ModeArgs,
    /// jettys to create and export (one rjetty blob each): pair with the
    /// reader's --jetties; extra jettys cost nothing until imported
    #[arg(long, default_value_t = 1)]
    jetties: u32,
    /// size sweep the reader will run; the segment is sized to its maximum
    #[arg(long)]
    sizes: Option<String>,
    /// explicit buffer size (plain bytes or k/m/g suffixes, e.g. 512m;
    /// overrides --sizes-derived sizing)
    #[arg(long, value_parser = parse_buf_len)]
    buf_len: Option<usize>,
}

/// one-sided op benchmark knobs shared by read-urma/write-urma: latency mode
/// (--depth 1, serialized percentiles) or pipelined bandwidth mode
/// (--depth > 1)
#[derive(Args)]
struct OneSidedArgs {
    #[command(flatten)]
    mode: ModeArgs,
    /// the [desc] hex line printed by the peer's serve-urma ('-' reads one line from stdin)
    desc: String,
    #[command(flatten)]
    bench: BenchArgs,
    /// outstanding READs: 1 = latency mode (serialized, default); >1 = bandwidth
    /// mode - up to this many READs in flight, reported as avg MiB/s + Mops
    /// instead of latency percentiles; bounded by the device's jfs/jfc/jfr
    /// depth caps (the queues are created at the requested depth - small
    /// sizes need depth x size above the fabric's bandwidth-latency product,
    /// i.e. hundreds at 4K on fast fabrics)
    #[arg(long, default_value_t = 1)]
    depth: u32,
    /// parallel jettys in bandwidth mode (serve-urma must have exported at
    /// least this many rjetty blobs via its own --jetties): the sweep's READs
    /// are assigned round-robin, each local jetty paired with one imported
    /// remote jetty, all completions on one shared CQ. A per-jetty fabric
    /// IOPS ceiling is the other candidate for what caps small-size Mops —
    /// if --post-list alone does not lift 4K, this is the knob
    #[arg(long, default_value_t = 1)]
    jetties: u32,
    /// READs per doorbell in bandwidth mode (perftest's post_list): this many
    /// work requests are chained into ONE post call, amortizing the per-post
    /// queue lock + doorbell that otherwise caps a single-threaded poster at
    /// ~2 Mops. Clamped to 1..=depth; try 8..64 at small sizes
    #[arg(long, default_value_t = 1)]
    post_list: u32,
    /// CQ moderation, bandwidth mode only: only every Nth READ generates a
    /// completion record (N is clamped to 1..=depth; perftest's cq_mod).
    /// 0 = auto: min(100, depth) at every size - per-op completion
    /// processing is what caps the ops rate of a full pipeline, small or
    /// large. Window/op accounting stays exact: a signaled READ is
    /// comp-ordered, and a tail without a record is closed by a 1-byte
    /// fence READ
    #[arg(long, default_value_t = 0)]
    cq_mod: u64,
    /// landing-buffer ceiling for bandwidth mode, bytes (plain or k/m/g
    /// suffixes); size x depth is capped here (default 1g) with overlapping
    /// landing windows when it bites - raise it for disjoint landings at
    /// big size x depth
    #[arg(long, value_parser = parse_buf_len)]
    landing_cap: Option<usize>,
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
    /// explicit buffer size (plain bytes or k/m/g suffixes, e.g. 512m;
    /// overrides --sizes-derived sizing)
    #[arg(long, value_parser = parse_buf_len)]
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
        Cmd::ReadUrma(a) => one_sided_run(a, BwOp::Read),
        Cmd::WriteUrma(a) => one_sided_run(a, BwOp::Write),
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
    if a.jetties == 0 || a.jetties > BW_MAX_JETTIES {
        return Err(Error::Invalid(format!("--jetties must be 1..={BW_MAX_JETTIES}")));
    }
    let (mode, tp, multi_path, cap) = preflight(&a.mode)?;
    let priority = resolve_priority(&a.mode, &cap, tp);
    let buf_len = resolve_buf_len(a.buf_len, a.sizes.as_deref())?;

    println!("[1/5] urma init + context on {}", a.mode.dev);
    let urma = Urma::init()?;
    let ctx = Context::create(&urma, &a.mode.dev)?;
    println!("      context eid {}", ctx.eid());

    /* one jfc shared by every jetty: the reader's multi-jetty mode reaps
       all completions from one queue (serve-urma itself never sees one) */
    println!(
        "[2/5] completion queue (depth {DEFAULT_DEPTH}) + {} jetty(ies), jfs priority {priority} \
               for {tp}",
        a.jetties
    );
    let cq = CompletionQueue::new(&ctx, DEFAULT_DEPTH)?;
    let mut jetties = Vec::with_capacity(a.jetties as usize);
    for _ in 0..a.jetties {
        jetties.push(Jetty::new(
            &ctx,
            &cq,
            JettyOpts { trans_mode: mode, multi_path, priority, ..Default::default() },
        )?);
    }
    for j in &jetties {
        println!("      jetty id {} uasid {:#x}", j.id().id, j.id().uasid);
    }

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
    let mut rjetty = Vec::with_capacity(jetties.len());
    for j in &jetties {
        rjetty.push(j.export_rjetty()?);
    }
    println!(
        "      seg-ctx {} bytes, {} x rjetty {} bytes",
        seg_ctx.len(),
        rjetty.len(),
        rjetty.first().map(|b| b.len()).unwrap_or(0)
    );

    println!("[5/5] descriptor for the peer (one hex line):");
    println!("[desc] {}", pack_desc(&WireDesc { seg, seg_ctx, rjetty }));

    /* one-sided READs/WRITEs never involve this CPU and raise no completion
       here; the only remaining job is keeping the resources alive */
    if std::io::stdin().is_terminal() {
        println!("[serve-urma] holding the segment for the peer's READs/WRITEs - press Enter to exit");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    } else {
        println!("[serve-urma] holding the segment for the peer's READs/WRITEs (stdin not a tty: park until killed)");
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }
    println!("[serve-urma] bye");
    Ok(())
}

/* ========================== urma: one-sided ops ========================== */

/// read-urma / write-urma: identical flow, one BwOp apart — the cap filter
/// (max_read_size vs max_write_size; WRITE additionally needs the READ
/// fallback for its verify read-back), the verify pass direction, and the
/// work request itself. Everything else (descriptor import, preflight,
/// depth/jetties/post-list/cq-mod plumbing, rotation, reporting) is shared.
fn one_sided_run(a: &OneSidedArgs, op: BwOp) -> Result<()> {
    let tag = op.tag();
    let mut sizes = bench_sizes(&a.bench)?;
    if a.depth == 0 {
        return Err(Error::Invalid("--depth must be at least 1".into()));
    }
    if a.duration > 0 && a.depth == 1 {
        return Err(Error::Invalid(
            "--duration needs bandwidth mode: pass --depth > 1 (latency mode counts iterations, \
             not seconds)"
                .into(),
        ));
    }
    if a.cq_mod > 0 && a.depth == 1 {
        return Err(Error::Invalid(
            "--cq-mod needs bandwidth mode: pass --depth > 1 (a serialized op cannot skip \
             completion records)"
                .into(),
        ));
    }
    if (a.jetties > 1 || a.post_list > 1) && a.depth == 1 {
        return Err(Error::Invalid(
            "--jetties/--post-list need bandwidth mode: pass --depth > 1 (latency mode is one \
             serialized op at a time)"
                .into(),
        ));
    }
    if a.jetties == 0 || a.jetties > BW_MAX_JETTIES {
        return Err(Error::Invalid(format!("--jetties must be 1..={BW_MAX_JETTIES}")));
    }
    if a.post_list == 0 || a.post_list > a.depth {
        return Err(Error::Invalid(format!(
            "--post-list must be 1..=depth ({}): a chained work-request list longer than the \
             pipeline would overflow the send queue",
            a.depth
        )));
    }
    let duration = (a.duration > 0).then_some(a.duration);

    println!("[{tag}] unpack peer descriptor");
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
    /* depth is bounded by the device's per-queue ceilings only (a 0 cap
       field = not reported, ignored): the CQ/jetty below are created at the
       requested depth, because saturating small sizes needs depth x size
       above the fabric's bandwidth-latency product — far more than 64
       outstanding 4K READs on a fast fabric */
    let depth_limit = [cap.max_jfs_depth, cap.max_jfc_depth, cap.max_jfr_depth]
        .into_iter()
        .filter(|&c| c > 0)
        .map(u64::from)
        .min()
        .unwrap_or(u64::from(DEFAULT_DEPTH));
    if u64::from(a.depth) > depth_limit {
        return Err(Error::Invalid(format!(
            "--depth {} exceeds the device's queue ceilings (jfs {} / jfc {} / jfr {}); \
             use at most {depth_limit}",
            a.depth, cap.max_jfs_depth, cap.max_jfc_depth, cap.max_jfr_depth
        )));
    }
    /* + jetties of headroom over the in-flight budget: the per-jetty fence
       READs at a drain sit on top of a full pipeline without overflowing
       any single queue */
    let qdepth = a.depth.max(DEFAULT_DEPTH) + a.jetties;
    /* sizes above the op's ceiling are skipped with a note, not fatal: one
       sweep then works on any device. A one-sided op is bounded by the
       device's max_read_size / max_write_size — max_msg_size caps
       two-sided messages, not one-sided ops (urma_device_cap_t carries all
       of them); 0 = not reported, then fall back to max_msg_size, then to
       no limit. WRITE's verify pass reads the range back, so it needs BOTH
       ceilings */
    let op_cap = |v: u64| if v != 0 { v } else if cap.max_msg_size != 0 { cap.max_msg_size } else { u64::MAX };
    let limit = match op {
        BwOp::Read => op_cap(cap.max_read_size).min(wire.seg.len),
        BwOp::Write => op_cap(cap.max_read_size).min(op_cap(cap.max_write_size)).min(wire.seg.len),
    };
    sizes.retain(|&s| {
        let ok = (s as u64) <= limit;
        if !ok {
            println!(
                "[{tag}] skip size {s}: above the {limit}-byte {} ceiling (peer segment {}, device max_read_size {} / max_write_size {} / max_msg_size {})",
                op.label(),
                wire.seg.len,
                cap.max_read_size,
                cap.max_write_size,
                cap.max_msg_size
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

    println!("[2/4] own completion queue (depth {qdepth}) + {} jetty(ies)", a.jetties);
    let priority = resolve_priority(&a.mode, &cap, tp);
    println!("      jfs priority {priority} for {tp}");
    let cq = CompletionQueue::new(&ctx, qdepth)?;
    let mut jetties = Vec::with_capacity(a.jetties as usize);
    for _ in 0..a.jetties {
        jetties.push(Jetty::new(
            &ctx,
            &cq,
            JettyOpts { depth: qdepth, trans_mode: mode, multi_path, priority, ..Default::default() },
        )?);
    }
    for j in &jetties {
        println!("      jetty id {} uasid {:#x}", j.id().id, j.id().uasid);
    }

    println!("[3/4] import {} peer(s) via blobs ({tp})", a.jetties);
    let want = a.jetties as usize;
    if wire.rjetty.len() < want {
        return Err(Error::Invalid(format!(
            "--jetties {want} but the descriptor carries {} rjetty blob(s): restart serve-urma \
             with --jetties {want} (it exports one blob per jetty)",
            wire.rjetty.len()
        )));
    }
    if wire.rjetty.len() > want {
        println!(
            "[{tag}] note: descriptor carries {} rjetty blobs, using the first {want}",
            wire.rjetty.len()
        );
    }
    let mut peers = Vec::with_capacity(want);
    for rj in &wire.rjetty[..want] {
        peers.push(Peer::import_ctx(&ctx, &wire.seg_ctx, rj, tp, TOKEN_VALUE)?);
    }
    /* latency/verify paths always run on jetty 0 */
    let (jetty, peer) = (&jetties[0], &peers[0]);

    let max_size = *sizes.last().unwrap();
    /* bandwidth mode registers size x depth of landing (capped) so the
       pipeline gets disjoint landing windows; the remote end rotates the
       peer's segment — give serve-urma a --buf-len above sweep-max x depth
       to widen that rotation too: fewer remote windows than the depth means
       concurrent ops hammer one remote range and the measurement
       saturates that memory, not the link */
    let landing_len = if a.depth > 1 {
        let want = max_size.saturating_mul(a.depth as usize);
        let capped = want.min(a.landing_cap.unwrap_or(BW_LANDING_CAP));
        if capped < want {
            let windows = (capped / max_size).max(1);
            println!(
                "[{tag}] note: landing capped at {capped} bytes ({max_size} x depth {} would be {want}); \
                 the {} in-flight ops share {windows} landing windows - raise --landing-cap for disjoint landings",
                a.depth, a.depth
            );
        }
        let remote_windows = wire.seg.len / max_size as u64;
        if remote_windows < u64::from(a.depth) {
            println!(
                "[{tag}] note: remote rotation covers {remote_windows} x {max_size}-byte windows, \
                 below depth {} - raise serve-urma --buf-len so concurrent ops do not hammer one remote range",
                a.depth
            );
        }
        println!(
            "[{tag}] bandwidth mode: depth {}, {} jetty(ies), post-list {}, cq-mod {}, landing {capped} bytes, remote rotation within the {}-byte peer segment",
            a.depth,
            a.jetties,
            a.post_list,
            if a.cq_mod == 0 { "auto".to_string() } else { a.cq_mod.to_string() },
            wire.seg.len
        );
        capped
    } else {
        max_size
    };
    println!(
        "[4/4] register {} buffer ({landing_len} bytes), sweep {} sizes: {}",
        if op == BwOp::Read { "landing" } else { "source" },
        sizes.len(),
        sizes.iter().map(|s| s.to_string()).collect::<Vec<_>>().join(",")
    );
    let mut landing = RegisteredBuf::new(&ctx, landing_len, TOKEN_VALUE)?;
    let remote_va = wire.seg.va;
    if op == BwOp::Write {
        println!("[{tag}] note: WRITEs overwrite the peer's pattern - restart serve-urma before a read-urma verify against the same segment");
    }

    let bwlabel = BwLabel { depth: a.depth, jetties: a.jetties, post_list: a.post_list };
    print_header("urma", op.label(), a.bench.iters, a.bench.warmup, &bwlabel, a.duration, a.bench.csv);
    for &size in &sizes {
        /* verify pass outside the timed loop, one serialized op at a time —
           kept at depth 1 even in bandwidth mode (a correctness pass must
           not overlap in-flight ops). READ pulls the peer's pattern and
           compares; WRITE pushes the pattern, then a READ-back compares:
           zero the window first so the comparison can't pass on bytes the
           source itself left behind (the sge borrows are kept inside each
           post call — the fill needs the buffer exclusive) */
        match op {
            BwOp::Read => {
                jetty.post_read(peer, remote_va, &[landing.sge(0, size as u32)?], READ_CTX)?;
                let _ = wait_read_spin(&cq)?;
                check_pat(&format!("size {size} verify read"), &landing[..size])?;
            }
            BwOp::Write => {
                fill_pat(&mut landing[..size]);
                jetty.post_write(peer, remote_va, &[landing.sge(0, size as u32)?], READ_CTX)?;
                let _ = wait_read_spin(&cq)?;
                landing[..size].fill(0);
                jetty.post_read(peer, remote_va, &[landing.sge(0, size as u32)?], READ_CTX)?;
                let _ = wait_read_spin(&cq)?;
                check_pat(&format!("size {size} verify write read-back"), &landing[..size])?;
            }
        }
        if a.depth == 1 {
            let sge = landing.sge(0, size as u32)?;
            let s = run_iters(
                || {
                    match op {
                        BwOp::Read => jetty.post_read(peer, remote_va, &[sge], READ_CTX)?,
                        BwOp::Write => jetty.post_write(peer, remote_va, &[sge], READ_CTX)?,
                    }
                    wait_read_spin(&cq).map(|_| ())
                },
                a.bench.warmup,
                a.bench.iters,
            )?;
            print_row("urma", size, a.bench.iters, &s, a.bench.csv);
        } else {
            let m = resolve_cq_mod(a.cq_mod, a.depth);
            let bw = BwCtx {
                jetties: &jetties,
                cq: &cq,
                peers: &peers,
                remote_va,
                seg_len: wire.seg.len,
                landing: &landing,
                post_list: a.post_list,
                op,
            };
            let s =
                run_bw_iters(&bw, size, a.depth, m, a.bench.warmup, a.bench.iters, duration)?;
            print_bw_row("urma", size, &bwlabel, m, &s, a.bench.csv);
        }
    }
    let per = if a.depth > 1 && a.duration > 0 {
        format!("{}s window", a.duration)
    } else {
        format!("{} iters", a.bench.iters)
    };
    println!("[{tag}] done: {} sizes, {per} each", sizes.len());
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

/// which one-sided op the sweep runs: READ pulls the peer's segment into
/// the landing buffer, WRITE pushes the landing buffer (the source) into
/// the peer's segment (perftest read_bw / write_bw). All the pipelining,
/// moderation, fence and rotation mechanics are op-agnostic — only the
/// work request itself differs.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BwOp {
    Read,
    Write,
}

impl BwOp {
    /// subcommand-style log prefix
    fn tag(&self) -> &'static str {
        match self {
            BwOp::Read => "read-urma",
            BwOp::Write => "write-urma",
        }
    }

    /// table/csv label
    fn label(&self) -> &'static str {
        match self {
            BwOp::Read => "READ",
            BwOp::Write => "WRITE",
        }
    }
}

/// the fixed parameters shared by the pipelined bandwidth passes
struct BwCtx<'a> {
    /// one per --jetties, all sharing the one CQ; op i runs on
    /// `bw_op_jetty(i)` paired with `peers[jetty]`
    jetties: &'a [Jetty],
    cq: &'a CompletionQueue,
    peers: &'a [Peer],
    remote_va: u64,
    /// peer segment length: the remote rotation window
    seg_len: u64,
    landing: &'a RegisteredBuf,
    /// READs chained per post call (--post-list)
    post_list: u32,
    /// which one-sided op a pass posts
    op: BwOp,
}

/// jetty of global op `i`: chunks of `list` consecutive ops per jetty,
/// cycling through `n` jettys (list=1 is plain round-robin). Chunking keeps
/// post-list batching and jetty parallelism composable: one chunk is one
/// linked work-request list on one jetty.
fn bw_op_jetty(i: u64, n: u64, list: u64) -> usize {
    ((i / list) % n) as usize
}

/// rank of global op `i` within its own jetty's op stream — what CQ
/// moderation counts against, since completions order per jetty. With n=1
/// (or list|n collapsing to a single stream) it is `i` itself.
fn bw_op_rank(i: u64, n: u64, list: u64) -> u64 {
    (i / (list * n)) * list + i % list
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

/// one pipelined pass over `bw.jetties` streams: post chunks while the
/// pipeline (global in-flight budget `depth`) is not full and the stop
/// condition allows, reap completions in `BW_POLL_BATCH` batches as they
/// land (each one frees slots for the next chunks). Ops carry
/// `user_ctx = global op index`; with CQ moderation `cq_mod = m` only ops
/// whose per-jetty rank is ≡ m-1 (mod m) are signaled, and a signaled op
/// carries comp_order, so a record proves every earlier op on THAT jetty
/// completed — `done[j]` tracks it per jetty. A jetty whose stream stops on
/// an unsignaled tail gets one extra 1-byte signaled fence READ
/// (`BW_FENCE_CTX_BASE - j`) whose record closes it — a READ also in WRITE
/// mode: comp_order is a jfs-wide flag, so the fence's record covers the
/// jetty's WRITE tail; fence bytes are not
/// counted, so op/byte accounting is exact at any moderation and jetty
/// count. A timed pass returns its whole first-post→last-completion window
/// plus the completed-op count. Completions only need counting — no per-op
/// timestamps: in a full pipeline every post→completion window contains
/// queueing time, so per-op figures are pipeline noise, not fabric
/// properties. The hang guard resets on every completion: it bounds
/// silence, not the pass (a duration pass runs long by design).
fn bw_pass(
    bw: &BwCtx, size: usize, depth: u32, cq_mod: u64, stop: &BwStop, timed: bool,
) -> Result<Option<(Duration, u64)>> {
    let n = bw.jetties.len() as u64;
    let list = u64::from(bw.post_list);
    let mut next = 0u64;
    let mut posted = vec![0u64; n as usize];
    let mut done = vec![0u64; n as usize];
    let mut done_total = 0u64;
    let mut fenced = vec![false; n as usize];
    let mut t0 = None;
    let mut deadline = Instant::now() + READ_TIMEOUT;
    let mut crs = [Completion { status: 0, user_ctx: 0, completion_len: 0 }; BW_POLL_BATCH];
    loop {
        while !stop.stop_posting(next) && next - done_total < u64::from(depth) {
            /* one chunk = the rest of the current post-list window, all on
               one jetty: a truncated previous chunk continues, not starts */
            let j = bw_op_jetty(next, n, list);
            let rem = (list - next % list) as usize;
            let mut rreqs = Vec::with_capacity(rem);
            let mut wreqs = Vec::with_capacity(rem);
            while match bw.op {
                BwOp::Read => rreqs.len(),
                BwOp::Write => wreqs.len(),
            } < rem
                && !stop.stop_posting(next)
                && next - done_total < u64::from(depth)
            {
                let rank = bw_op_rank(next, n, list);
                let off = bw_slot_off(next, size, bw.landing.len());
                let va = bw.remote_va + bw_slot_off(next, size, bw.seg_len as usize) as u64;
                match bw.op {
                    BwOp::Read => rreqs.push(ReadReq {
                        remote_va: va,
                        local: bw.landing.sge(off, size as u32)?,
                        user_ctx: next,
                        signaled: (rank + 1).is_multiple_of(cq_mod),
                    }),
                    BwOp::Write => wreqs.push(WriteReq {
                        remote_va: va,
                        local: bw.landing.sge(off, size as u32)?,
                        user_ctx: next,
                        signaled: (rank + 1).is_multiple_of(cq_mod),
                    }),
                }
                if timed && next == 0 {
                    t0 = Some(Instant::now());
                }
                next += 1;
                posted[j] += 1;
            }
            match bw.op {
                BwOp::Read => bw.jetties[j].post_read_list(&bw.peers[j], &rreqs)?,
                BwOp::Write => bw.jetties[j].post_write_list(&bw.peers[j], &wreqs)?,
            }
        }
        /* posting has stopped: once a queue slot frees up, fence every
           jetty that ended on an unsignaled tail, so the window can still
           close on records that prove all posted ops completed */
        if stop.stop_posting(next) && next - done_total < u64::from(depth) {
            for j in 0..n as usize {
                if !fenced[j] && bw_tail_uncovered(posted[j], cq_mod) {
                    bw.jetties[j].post_read_signaled(
                        &bw.peers[j],
                        bw.remote_va,
                        &[bw.landing.sge(0, 1)?],
                        BW_FENCE_CTX_BASE - j as u64,
                        true,
                    )?;
                    fenced[j] = true;
                }
            }
        }
        if done_total == next && stop.stop_posting(next) {
            break;
        }
        let cnt = bw.cq.poll_batch(&mut crs)?;
        if cnt == 0 {
            if Instant::now() >= deadline {
                return Err(Error::PollTimeout { user_ctx: next });
            }
            std::hint::spin_loop();
            continue;
        }
        for cr in &crs[..cnt] {
            if !cr.is_success() {
                return Err(Error::BadCompletion { status: cr.status, user_ctx: cr.user_ctx });
            }
            let ctx = cr.user_ctx;
            let (j, target) = if ctx >= BW_FENCE_CTX_MIN {
                let j = (BW_FENCE_CTX_BASE - ctx) as usize;
                (j, posted[j])
            } else {
                let j = bw_op_jetty(ctx, n, list);
                (j, bw_op_rank(ctx, n, list) + 1)
            };
            if target > done[j] {
                done_total += target - done[j];
                done[j] = target;
            }
            deadline = Instant::now() + READ_TIMEOUT;
        }
    }
    Ok(t0.map(|t0| (t0.elapsed(), done_total)))
}

/// effective CQ moderation for one pass: 0 (default) = auto — perftest's
/// min(100, depth) at EVERY size (per-op completion processing is what caps
/// the ops rate of a full pipeline, from a 4K sweep to a ~0.5-Mops 64 KiB
/// one — the poster and the CQ reaper share one thread in bw_pass). An
/// explicit value overrides the rule, clamped to 1..=depth (perftest clamps
/// cq_mod to the queue depth too; above it a full pipeline would hold no
/// signaled op to learn completion from). Refill granularity note:
/// done-counting advances in cq_mod-sized jumps, so the in-flight window
/// bottoms out near depth - cq_mod — keep depth comfortably above the
/// moderation when chasing peak bandwidth.
fn resolve_cq_mod(flag: u64, depth: u32) -> u64 {
    if flag == 0 {
        BW_CQ_MOD_AUTO.min(u64::from(depth))
    } else {
        flag.clamp(1, u64::from(depth))
    }
}

/// whether a jetty stream of `posts` ops ends on an unsignaled one — the
/// tail a fence READ must cover: moderation on, and the stream not a whole
/// number of groups
fn bw_tail_uncovered(posts: u64, cq_mod: u64) -> bool {
    cq_mod > 1 && !posts.is_multiple_of(cq_mod)
}

/// bandwidth counterpart of `run_iters`: an untimed warmup pass, then the
/// timed one. Op-count mode uses --warmup/--iters; duration mode warms up a
/// fixed 1s and then runs to a seconds-long deadline — a window long enough
/// that one scheduler hiccup cannot dominate the average.
fn run_bw_iters(
    bw: &BwCtx, size: usize, depth: u32, cq_mod: u64, warmup: u32, iters: u32,
    duration: Option<u64>,
) -> Result<BwStats> {
    let (warm_stop, main_stop) = match duration {
        Some(secs) => (
            BwStop::Until(Instant::now() + BW_WARMUP),
            BwStop::Until(Instant::now() + Duration::from_secs(secs)),
        ),
        None => (BwStop::Ops(u64::from(warmup)), BwStop::Ops(u64::from(iters))),
    };
    if matches!(warm_stop, BwStop::Ops(n) if n > 0) {
        bw_pass(bw, size, depth, cq_mod, &warm_stop, false)?;
    }
    let (window, ops) = bw_pass(bw, size, depth, cq_mod, &main_stop, true)?
        .expect("timed pass posts at least one op");
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

/// jfs priority slot for the run: an explicit --priority wins, else the
/// device's slot for the selected tp type (perftest's auto -O resolution),
/// else the wrapper's 15 default with a note — the stream then runs in
/// whatever service class that slot maps to
fn resolve_priority(m: &ModeArgs, cap: &DeviceCap, tp: TpType) -> u8 {
    if let Some(p) = m.priority {
        return p;
    }
    match cap.priority_for(tp) {
        Some(p) => p,
        None => {
            println!(
                "[mode] note: the device's priority table has no {tp} slot; jfs priority stays at \
                 the {URMA_MAX_PRIORITY} default - pass --priority to pin a slot explicitly",
            );
            URMA_MAX_PRIORITY
        }
    }
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
    print_header(
        "tcp",
        "READ",
        a.bench.iters,
        a.bench.warmup,
        &BwLabel { depth: 1, jetties: 1, post_list: 1 },
        0,
        a.bench.csv,
    );
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
        let first = parse_one_size(first, "--sizes")?;
        let last = parse_one_size(last, "--sizes")?;
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
        sizes.push(parse_one_size(part, "--sizes")?);
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
fn parse_one_size(tok: &str, what: &str) -> Result<usize> {
    let tok = tok.trim();
    let (num, mult) = match tok.as_bytes().last() {
        Some(b'k' | b'K') => (&tok[..tok.len() - 1], 1 << 10),
        Some(b'm' | b'M') => (&tok[..tok.len() - 1], 1 << 20),
        Some(b'g' | b'G') => (&tok[..tok.len() - 1], 1 << 30),
        _ => (tok, 1),
    };
    let n: usize =
        num.parse().map_err(|_| Error::Invalid(format!("bad size '{tok}' in {what}")))?;
    let n = n.checked_mul(mult).ok_or_else(|| Error::Invalid(format!("size '{tok}' overflows")))?;
    if n == 0 {
        return Err(Error::Invalid(format!("{what} contains 0")));
    }
    if n > u32::MAX as usize {
        return Err(Error::Invalid(format!("size {n} in {what} exceeds the u32 sge length limit")));
    }
    Ok(n)
}

/// clap value parser for --buf-len: the same tokens as --sizes entries
/// ("512m" reads better than 536870912)
fn parse_buf_len(s: &str) -> std::result::Result<usize, String> {
    parse_one_size(s, "--buf-len").map_err(|e| e.to_string())
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

/// the bandwidth knobs the printers need, bundled to keep their signatures
/// small (latency mode passes all-1s)
struct BwLabel {
    depth: u32,
    jetties: u32,
    post_list: u32,
}

fn print_header(
    transport: &str, op: &str, iters: u32, warmup: u32, bw: &BwLabel, duration: u64, csv: bool,
) {
    if csv {
        if bw.depth > 1 {
            println!("csv,transport,size_bytes,ops,depth,cq_mod,bw_avg_mib,mops,jetties,post_list");
        } else {
            println!("csv,transport,size_bytes,iters,min_us,p50_us,avg_us,p99_us,max_us");
        }
    } else if bw.depth > 1 {
        let mode = if duration > 0 {
            format!("{duration}s window + 1s warmup")
        } else {
            format!("{iters} iters + {warmup} warmup")
        };
        let parallel = if bw.jetties > 1 || bw.post_list > 1 {
            format!(", {} jetty(ies), post-list {}", bw.jetties, bw.post_list)
        } else {
            String::new()
        };
        println!(
            "== {transport} {op} bandwidth: depth {}{parallel}, {mode} per size, MiB/s ==",
            bw.depth
        );
        println!("{:>10} {:>7} {:>10} {:>10}", "size", "cq-mod", "avg", "Mops");
    } else {
        println!("== {transport} {op} latency: {iters} iters + {warmup} warmup per size, µs ==");
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

fn print_bw_row(
    transport: &str, size: usize, bw: &BwLabel, cq_mod: u64, s: &BwStats, csv: bool,
) {
    if csv {
        println!(
            "csv,{transport},{size},{},{},{cq_mod},{:.2},{:.3},{},{}",
            s.ops, bw.depth, s.avg, s.mops, bw.jetties, bw.post_list
        );
    } else {
        println!("{:>10} {:>7} {:>10.2} {:>10.3}", size, cq_mod, s.avg, s.mops);
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
/// length ceiling, the eid for the loopback guard), the seg-ctx import blob
/// and one rjetty blob per serve-side jetty (serve-urma --jetties N exports
/// N; the reader pairs its own N jettys with them one-to-one).
struct WireDesc {
    seg: SegDesc,
    seg_ctx: Vec<u8>,
    rjetty: Vec<Vec<u8>>,
}

/// little-endian: eid[16] uasid va len attr token_id
/// seg_ctx_len u32 rjetty_cnt u32 rjetty_len[cnt] u32 seg_ctx blobs...
fn pack_desc(d: &WireDesc) -> String {
    let seg = &d.seg;
    let rj_total: usize = d.rjetty.iter().map(|b| b.len()).sum();
    let mut b = Vec::with_capacity(52 + 4 * d.rjetty.len() + d.seg_ctx.len() + rj_total);
    b.extend_from_slice(&seg.eid.0);
    b.extend_from_slice(&seg.uasid.to_le_bytes());
    b.extend_from_slice(&seg.va.to_le_bytes());
    b.extend_from_slice(&seg.len.to_le_bytes());
    b.extend_from_slice(&seg.attr.to_le_bytes());
    b.extend_from_slice(&seg.token_id.to_le_bytes());
    b.extend_from_slice(&(d.seg_ctx.len() as u32).to_le_bytes());
    b.extend_from_slice(&(d.rjetty.len() as u32).to_le_bytes());
    for rj in &d.rjetty {
        b.extend_from_slice(&(rj.len() as u32).to_le_bytes());
    }
    b.extend_from_slice(&d.seg_ctx);
    for rj in &d.rjetty {
        b.extend_from_slice(rj);
    }
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
    let rj_cnt = rd.u32("rjetty count")? as usize;
    if rj_cnt == 0 {
        return Err(Error::Invalid("descriptor carries no rjetty blob".into()));
    }
    let rj_lens = (0..rj_cnt)
        .map(|i| Ok(rd.u32(&format!("rjetty length #{i}"))? as usize))
        .collect::<std::result::Result<Vec<_>, Error>>()?;
    let seg_ctx = rd.take(seg_len, "seg ctx")?.to_vec();
    let mut rjetty = Vec::with_capacity(rj_cnt);
    for (i, &len) in rj_lens.iter().enumerate() {
        rjetty.push(rd.take(len, &format!("rjetty #{i}"))?.to_vec());
    }
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
            rjetty: vec![vec![0xbb; 55]],
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

        /* multi-jetty descriptor: N rjetty blobs survive the round trip and
           keep their order (jetty i pairs with blob i) */
        let d = WireDesc {
            seg: d.seg,
            seg_ctx: vec![0xaa; 8],
            rjetty: vec![vec![1; 3], vec![2; 5], vec![3; 7]],
        };
        let rt = unpack_desc(&pack_desc(&d)).expect("roundtrip");
        assert_eq!(rt.rjetty, d.rjetty);
        assert_eq!(rt.seg_ctx, d.seg_ctx);
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
    fn buf_len_suffix_parse() {
        assert_eq!(parse_buf_len("512m").unwrap(), 512 << 20);
        assert_eq!(parse_buf_len("268435456").unwrap(), 268435456);
        assert_eq!(parse_buf_len("1G").unwrap(), 1 << 30);
        assert!(parse_buf_len("512x").is_err());
        assert!(parse_buf_len("0").is_err()); /* "--buf-len contains 0" */
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

    #[test]
    fn bw_cq_mod_resolution() {
        /* auto: min(100, depth) at every size — moderation is no longer
           size-gated, a full 64 KiB pipeline reaps ~0.5 Mops too */
        assert_eq!(resolve_cq_mod(0, 1), 1);
        assert_eq!(resolve_cq_mod(0, 64), 64);
        assert_eq!(resolve_cq_mod(0, 100), 100);
        assert_eq!(resolve_cq_mod(0, 512), 100);
        /* explicit: clamped to 1..=depth (above it a full pipeline would
           hold no signaled op to learn completion from) */
        assert_eq!(resolve_cq_mod(1, 64), 1);
        assert_eq!(resolve_cq_mod(1000, 64), 64);
        assert_eq!(resolve_cq_mod(8, 64), 8);
    }

    #[test]
    fn bw_fence_tail_rule() {
        assert!(!bw_tail_uncovered(1000, 1)); /* moderation off: every op signaled */
        assert!(!bw_tail_uncovered(1000, 100)); /* whole groups: last op signaled */
        assert!(bw_tail_uncovered(1050, 100)); /* 50-op tail left unsignaled */
        assert!(bw_tail_uncovered(1, 100)); /* a single op with moderation on */
        assert!(!bw_tail_uncovered(0, 100)); /* nothing posted: nothing to cover */
    }

    #[test]
    fn bw_op_assignment() {
        /* single jetty: identity mapping whatever the post-list */
        for i in 0..50u64 {
            assert_eq!(bw_op_jetty(i, 1, 1), 0);
            assert_eq!(bw_op_jetty(i, 1, 8), 0);
            assert_eq!(bw_op_rank(i, 1, 1), i);
            assert_eq!(bw_op_rank(i, 1, 8), i);
        }
        /* round-robin (list=1): op i on jetty i%n, ranks dense per jetty */
        let n = 4;
        let mut seen = [0u64; 4];
        for i in 0..40u64 {
            let j = bw_op_jetty(i, n, 1);
            assert_eq!(j, (i % n) as usize);
            assert_eq!(bw_op_rank(i, n, 1), seen[j]);
            seen[j] += 1;
        }
        assert_eq!(seen, [10; 4]);
        /* chunked (list=3): runs of 3 ops per jetty; ranks stay dense and
           continue correctly across chunk boundaries */
        let (n, list) = (2u64, 3u64);
        let mut seen = [0u64; 2];
        for i in 0..30u64 {
            let j = bw_op_jetty(i, n, list);
            assert_eq!(j, ((i / list) % n) as usize);
            assert_eq!(bw_op_rank(i, n, list), seen[j]);
            seen[j] += 1;
        }
        assert_eq!(seen, [15; 2]);
    }
}
