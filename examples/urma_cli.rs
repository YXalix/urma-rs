//! Minimal pure-URMA CLI — the single-file usage example for the whole crate.
//!
//! Unlike urma_hello/pingpong/lookup there is no HTTP control plane (and no
//! tokio/serde): the segment/jetty descriptor travels by hand — `serve`
//! prints it as one hex line, you paste that line as `read`'s positional
//! argument on the other node. Each step is printed so the resource flow
//! reads top to bottom:
//!
//! - `list`  device discovery, `--caps` appends the supported-mode matrix
//! - `serve` context → cq → jetty → registered seg → exported blobs → hold
//! - `read`  own resources → unpack → `Peer::import_ctx` → `post_read` → CQ
//!
//! Two-node session (defaults are CTP-RM; `--mode`/`--tp` select any combo
//! the device advertises, both sides must pass the same values):
//!
//! ```bash
//! nodeA$ cargo run --example urma_cli -- serve -d bonding_dev_0
//! nodeB$ cargo run --example urma_cli -- read -d bonding_dev_0 '<[desc] hex>'
//! ```
//!
//! `serve` has no completion to wait for — a one-sided READ never involves
//! the remote CPU — so it only holds the segment alive (Enter exits when
//! stdin is a tty; from a script it parks until killed). Bonding devices
//! force multi-path on, matching the other examples.

use std::io::IsTerminal;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand, ValueEnum};
use urma_rs::{
    list_devices, query_device, CompletionQueue, Context, Eid, Error, Jetty, JettyOpts, Peer,
    RegisteredBuf, Result, SegDesc, TpType, TransMode, Urma, DEFAULT_DEPTH, PAGE_SIZE,
    TOKEN_VALUE,
};

/// user_ctx tag correlating the READ completion
const READ_CTX: u64 = 0x1234;
/// how much of the read data to hex-dump after the text line
const HEX_PREVIEW: usize = 32;

#[derive(Parser)]
#[command(
    name = "urma_cli",
    version,
    about = "Minimal pure-URMA CLI: device discovery, capability probe, and a manual \
             copy-paste one-sided READ between two nodes (no HTTP control plane)",
    after_help = "Two-node session (defaults are CTP-RM; --mode/--tp select any combo the \
                  device advertises, both sides must agree):\n  \
                  nodeA$ urma_cli serve -d bonding_dev_0\n  \
                  nodeB$ urma_cli read  -d bonding_dev_0 '<the [desc] hex line>'"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// list devices, one per line; --caps appends each device's supported-mode matrix
    #[command(
        after_help = "The --caps block, per device:\n  \
                      modes  : every advertised transport mode (RM reliable message, RC reliable\n  \
                      connection, UM unreliable message) with the tp types it can use\n  \
                      (tp=RTP,CTP,...), its order types (order=ot,oi,...) and multi-path;\n  \
                      RC[tp=] means the mode bit is set but no tp type is usable, so the\n  \
                      mode is effectively unavailable\n  \
                      combos : the (mode, tp) pairs usable as serve/read --mode/--tp values\n  \
                      (CTP additionally requires the device-level ctp_en gate)\n  \
                      limits : max_jfs_sge/max_jfr_sge = scatter-gather entries per\n  \
                      send/recv, max_msg_size = largest single message in bytes,\n  \
                      page_size_cap = page-size bitmap for pinned registration"
    )]
    List {
        #[arg(long)]
        caps: bool,
    },
    /// register memory, print the descriptor, hold it for the peer's READ
    Serve(ServeArgs),
    /// import the peer's descriptor and one-sided READ its memory
    Read(ReadArgs),
}

/// mode flags shared by serve/read (trans_mode is set at create, tp_type at
/// import; both peers must run the same combination)
#[derive(Args)]
struct ModeArgs {
    /// device name, as printed by `urma_cli list`
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

#[derive(Args)]
struct ServeArgs {
    #[command(flatten)]
    mode: ModeArgs,
    /// text published at seg offset 0 (zero-padded to the segment)
    #[arg(long)]
    msg: Option<String>,
    /// registered buffer size in bytes
    #[arg(long, default_value_t = PAGE_SIZE)]
    buf_len: usize,
}

#[derive(Args)]
struct ReadArgs {
    #[command(flatten)]
    mode: ModeArgs,
    /// the [desc] hex line printed by the peer's `serve` ('-' reads one line from stdin)
    desc: String,
    /// byte offset into the peer's segment
    #[arg(long, default_value_t = 0)]
    offset: u64,
    /// bytes to read
    #[arg(long, default_value_t = 64)]
    len: u64,
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
        Cmd::List { caps } => list_run(*caps),
        Cmd::Serve(a) => serve_run(a),
        Cmd::Read(a) => read_run(a),
    };
    match r {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[main] error: {e}");
            ExitCode::FAILURE
        }
    }
}

/* ---- subcommand: list ---- */

fn list_run(caps: bool) -> Result<()> {
    let names = list_devices()?;
    if names.is_empty() {
        return Err(Error::Invalid("no urma device on this machine".into()));
    }
    for name in &names {
        println!("{name}");
        if caps {
            match query_device(name) {
                Ok(cap) => {
                    println!("  modes  : {cap}");
                    println!(
                        "  combos : {}",
                        cap.supported_combos()
                            .iter()
                            .map(|(m, tp)| format!("{m}-{tp}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    println!(
                        "  limits : max_jfs_sge {} max_jfr_sge {} max_msg_size {} page_size_cap {:#x}",
                        cap.max_jfs_sge, cap.max_jfr_sge, cap.max_msg_size, cap.page_size_cap
                    );
                }
                Err(e) => eprintln!("  query failed: {e}"),
            }
        }
    }
    if caps {
        println!(
            "  (legend: modes = transport modes RM/RC/UM with their usable tp types; an
   empty tp= means the mode is advertised but unusable here; combos = the valid
   --mode/--tp pairs; limits = per-send/recv sge, message-size and page-size
   ceilings. Background: docs/urma.md)"
        );
    }
    Ok(())
}

/* ---- mode preflight ---- */

/// Parameterized version of the fixed CTP-RM preflight the other examples
/// run (`common::check_mode_support`): query the device first, so an
/// unsupported combination fails with the supported-mode matrix instead of
/// an opaque jetty-creation / import error. Returns (trans_mode, tp_type,
/// multi_path) with bonding devices forcing multi-path on.
fn preflight(m: &ModeArgs) -> Result<(TransMode, TpType, bool)> {
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
    /* Bonding devices normally force multi-path on, but the cap bit comes
       straight from the NIC driver's registered attrs (the kernel never
       derives it) and some drivers leave it 0 even though single-path RM
       works — fall back instead of failing before any resource exists. */
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
    Ok((mode, tp, multi_path))
}

/* ---- subcommand: serve ---- */

fn serve_run(a: &ServeArgs) -> Result<()> {
    let (mode, _tp, multi_path) = preflight(&a.mode)?;
    let msg = a.msg.clone().unwrap_or_else(|| format!("hello from urma-cli@{}", hostname()));
    if msg.len() > a.buf_len {
        return Err(Error::Invalid(format!(
            "msg is {} bytes, exceeds buf-len {}",
            msg.len(),
            a.buf_len
        )));
    }

    println!("[1/5] urma init + context on {}", a.mode.dev);
    let urma = Urma::init()?;
    let ctx = Context::create(&urma, &a.mode.dev)?;
    println!("      context eid {}", ctx.eid());

    println!("[2/5] completion queue (depth {DEFAULT_DEPTH}) + jetty");
    let cq = CompletionQueue::new(&ctx, DEFAULT_DEPTH)?;
    let jetty =
        Jetty::new(&ctx, &cq, JettyOpts { trans_mode: mode, multi_path, ..Default::default() })?;
    println!("      jetty id {} uasid {:#x}", jetty.id().id, jetty.id().uasid);

    println!("[3/5] register {}-byte segment (token_value {TOKEN_VALUE:#x})", a.buf_len);
    let mut buf = RegisteredBuf::new(&ctx, a.buf_len, TOKEN_VALUE)?;
    let seg = buf.descriptor();
    buf[..msg.len()].copy_from_slice(msg.as_bytes());
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

    /* a one-sided READ never involves this CPU and raises no completion
       here; the only remaining job is keeping the resources alive */
    if std::io::stdin().is_terminal() {
        println!("[serve] holding the segment for the peer's READ - press Enter to exit");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
    } else {
        println!("[serve] holding the segment for the peer's READ (stdin not a tty: park until killed)");
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }
    println!("[serve] bye");
    Ok(())
}

/* ---- subcommand: read ---- */

fn read_run(a: &ReadArgs) -> Result<()> {
    let desc_hex = if a.desc == "-" {
        println!("[read ] reading descriptor line from stdin");
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map_err(Error::Io)?;
        line
    } else {
        a.desc.clone()
    };

    /* validate the pasted argument first: it is the most likely typo */
    println!("[read ] unpack peer descriptor");
    let wire = unpack_desc(&desc_hex)?;
    println!(
        "      peer seg eid {} uasid {:#x} va {:#x} len {} token_id {}",
        wire.seg.eid, wire.seg.uasid, wire.seg.va, wire.seg.len, wire.seg.token_id
    );
    if a.offset + a.len > wire.seg.len {
        return Err(Error::Invalid(format!(
            "offset {} + len {} exceeds the peer segment length {}",
            a.offset, a.len, wire.seg.len
        )));
    }
    let (mode, tp, multi_path) = preflight(&a.mode)?;

    println!("[1/4] urma init + context on {}", a.mode.dev);
    let urma = Urma::init()?;
    let ctx = Context::create(&urma, &a.mode.dev)?;
    println!("      context eid {}", ctx.eid());
    if wire.seg.eid == ctx.eid() {
        return Err(Error::Invalid(
            "peer eid equals this context's eid: single-machine loopback crashes inside \
             liburma's import path; run `serve` on another node"
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

    let remote_va = wire.seg.va + a.offset;
    let len = a.len as u32;
    println!("[4/4] register landing buffer + post READ of {len} bytes at remote va {remote_va:#x}");
    let landing = RegisteredBuf::new(&ctx, PAGE_SIZE.max(a.len as usize), TOKEN_VALUE)?;
    let sge = landing.sge(0, len)?;
    jetty.post_read(&peer, remote_va, &[sge], READ_CTX)?;
    let comp = cq.wait_read(READ_CTX)?;
    println!(
        "[read ] completion: status {} len {} (user_ctx {:#x})",
        comp.status, comp.completion_len, comp.user_ctx
    );

    /* READ completions carry no length on this provider (completion_len
       stays 0); the landing buffer holds whatever was asked for — same
       approach as urma_hello */
    let n = a.len as usize;
    let data = &landing[..n];
    println!("[read ] data: \"{}\"", String::from_utf8_lossy(data).trim_end_matches('\0'));
    println!("[read ] hex[..{HEX_PREVIEW}]: {}", hex_enc(&data[..n.min(HEX_PREVIEW)]));
    println!("[read ] bye");
    Ok(())
}

/* ---- wire descriptor: hand-rolled hex, the demo has no serde ---- */

/// Everything `read` needs on the other node: the plain [`SegDesc`] (the va
/// to read from, the eid for the loopback guard) plus the two import blobs.
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

fn hostname() -> String {
    let h = std::fs::read_to_string("/proc/sys/kernel/hostname")
        .unwrap_or_default()
        .trim()
        .to_string();
    if h.is_empty() {
        format!("pid-{}", std::process::id())
    } else {
        h
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
}
