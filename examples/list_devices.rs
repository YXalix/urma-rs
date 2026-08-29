//! Lists URMA devices on this machine (probe tool for `urma_get_device_list`).
//!
//! ```bash
//! cargo run --example list_devices            # names only
//! cargo run --example list_devices -- --caps  # + per-device capability matrix
//! ```
//!
//! stdout prints one device name per line (e.g. `udma2` / `bonding_dev_0`);
//! the other examples take these names as `--device`. With `--caps`, each
//! name is followed by an indented block (also stdout) reporting which
//! communication modes the device supports (`urma_query_device`):
//! - `modes`  : every advertised transport mode (RM reliable message, RC
//!   reliable connection, UM unreliable message) with the tp types it can
//!   use (tp=RTP,CTP,...), its order types (order=ot,oi,...) and
//!   multi-path; `RC[tp=]` means the mode bit is set but no tp type is
//!   usable, so the mode is effectively unavailable
//! - `combos` : the usable (mode, tp) pairs (CTP additionally requires the
//!   device-level ctp_en gate) — the answer to "which communication modes
//!   does this device support"
//! - `limits` : max_jfs_sge/max_jfr_sge = scatter-gather entries per
//!   send/recv, max_msg_size = largest single message in bytes,
//!   page_size_cap = page-size bitmap for pinned registration
//!
//! A legend trailer is printed after the listing; full background in
//! `docs/urma.md`. Exit code: 0 = at least one device, 1 = no device or
//! urma init failed.

use urma_rs::{list_devices, query_device};

fn main() {
    let show_caps = std::env::args().skip(1).any(|a| a == "--caps");
    match list_devices() {
        Ok(names) if !names.is_empty() => {
            for name in &names {
                println!("{name}");
                if show_caps {
                    match query_device(name) {
                        Ok(cap) => {
                            println!("  modes        : {cap}");
                            println!(
                                "  combos       : {}",
                                cap.supported_combos()
                                    .iter()
                                    .map(|(m, tp)| format!("{m}-{tp}"))
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            );
                            println!(
                                "  limits       : max_jfs_sge {} max_jfr_sge {} max_msg_size {} page_size_cap {:#x}",
                                cap.max_jfs_sge, cap.max_jfr_sge, cap.max_msg_size, cap.page_size_cap
                            );
                        }
                Err(e) => eprintln!("  query failed: {e}"),
            }
        }
    }
    if show_caps {
        println!(
            "  (legend: modes = transport modes RM/RC/UM with their usable tp types; an
   empty tp= means the mode is advertised but unusable here; combos = the usable
   mode-tp combinations; limits = per-send/recv sge, message-size and page-size
   ceilings. Background: docs/urma.md)"
        );
    }
}
        Ok(_) => {
            eprintln!("no urma device on this machine");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("urma unavailable on this machine: {e}");
            std::process::exit(1);
        }
    }
}
