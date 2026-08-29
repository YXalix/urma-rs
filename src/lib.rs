//! # urma-rs
//!
//! Rust wrapper for the URMA (Unified Remote Memory Access) userspace API.
//!
//! Layers:
//! - [`ffi`]: hand-written raw bindings (unsafe, transcribed from the vendored
//!   headers under `include/`, layout guarded by unit tests; links the system
//!   library liburma.so);
//! - [`error`] / [`urma`]: safe layer, RAII wrappers for URMA resources
//!   (context / jfc / jfr / jetty / registered memory / imported resources).
//!   Children hold reference-counted handles to their parents, so `Drop`
//!   always runs in a safe order regardless of drop sequence.

pub mod error;
pub mod ffi;
pub mod urma;

pub use error::{Error, Result};
pub use urma::{
    list_devices, Completion, CompletionQueue, Context, Eid, Jetty, JettyId, JettyOpts, LocalSge,
    PageBuf, Peer, RegisteredBuf, RegisteredSeg, SegDesc, Urma, DEFAULT_DEPTH, PAGE_SIZE,
    POLL_INTERVAL, POLL_RETRIES, TOKEN_VALUE,
};
