//! Lists URMA devices on this machine (probe tool for `urma_get_device_list`).
//!
//! ```bash
//! cargo run --example list_devices
//! ```
//!
//! stdout prints one device name per line (e.g. `udma2` / `bonding_dev_0`);
//! the other examples take these names as `--device`. Exit code: 0 = at least
//! one device, 1 = no device or urma init failed.

use urma_rs::list_devices;

fn main() {
    match list_devices() {
        Ok(names) if !names.is_empty() => {
            for name in &names {
                println!("{name}");
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
