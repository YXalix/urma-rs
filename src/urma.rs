//! Safe URMA wrapper layer.
//!
//! Each resource type maps to a pair of C APIs (create/delete or
//! register/unregister or import/unimport); `Drop` performs symmetric cleanup.
//! The resources form a tree (Urma -> Context -> CompletionQueue -> Jetty,
//! Context -> RegisteredSeg / Peer), and every child holds a reference-counted
//! handle to its parent: a parent's C object is deleted only after the last
//! child using it is gone, so destruction order is always safe no matter in
//! which order the handles are dropped. `Context` and `CompletionQueue` are
//! cheap cloneable handles (like dup'd fds), not unique owners.

use std::alloc::{alloc_zeroed, dealloc, Layout};
use std::cell::RefCell;
use std::ffi::CString;
use std::fmt;
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;

use crate::error::{check_status, Error, Result};
use crate::ffi;

/// NULL-returning FFI call failed: capture errno immediately, before any
/// cleanup call can clobber it (0 when the C library does not set one)
fn null_err(what: &'static str) -> Error {
    Error::Null(what, std::io::Error::last_os_error().raw_os_error().unwrap_or(0))
}

/// Plain token value agreed by both ends (security credential); this is the
/// `urma_token_t` value, unrelated to the token-id mechanism (which stays
/// disabled, see [`RegisteredSeg::register`])
pub const TOKEN_VALUE: u32 = 0xACFE;
pub const DEFAULT_DEPTH: u32 = 64;
/// Poll parameters for completion records: 100 retries x 100ms
pub const POLL_RETRIES: u32 = 100;
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Page size for [`PageBuf`] alignment and buffer sizing; URMA devices must
/// support at least 4K pages (`page_size_cap` in urma_types.h)
pub const PAGE_SIZE: usize = 4096;

/* ============================== Basic value types ============================== */

/// 16-byte EID (UB network endpoint identifier)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Eid(pub [u8; 16]);

impl Eid {
    fn from_raw(raw: ffi::urma_eid_t) -> Self {
        Eid(raw.raw)
    }

    fn to_raw(self) -> ffi::urma_eid_t {
        ffi::urma_eid_t { raw: self.0 }
    }
}

impl fmt::Display for Eid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for i in (0..16).step_by(2) {
            if i != 0 {
                write!(f, ":")?;
            }
            write!(f, "{:02x}{:02x}", self.0[i], self.0[i + 1])?;
        }
        Ok(())
    }
}

/// jetty identifier (published to the directory; used by the peer on import)
#[derive(Clone, Copy, Debug)]
pub struct JettyId {
    pub eid: Eid,
    pub uasid: u32,
    pub id: u32,
}

impl JettyId {
    fn from_raw(raw: &ffi::urma_jetty_id_t) -> Self {
        JettyId { eid: Eid::from_raw(raw.eid), uasid: raw.uasid, id: raw.id }
    }

    fn to_raw(self) -> ffi::urma_jetty_id_t {
        ffi::urma_jetty_id_t {
            eid: self.eid.to_raw(),
            uasid: self.uasid,
            id: self.id,
        }
    }
}

/// Public descriptor of registered memory: published to the directory as-is,
/// for the peer's `Peer::import`
#[derive(Clone, Copy, Debug)]
pub struct SegDesc {
    pub eid: Eid,
    pub uasid: u32,
    pub va: u64,
    pub len: u64,
    /// Raw `urma_seg_attr_t` value: opaque to users, round-tripped to
    /// `Peer::import` together with the descriptor
    pub attr: u32,
    /// Driver-allocated id from the remote's register; the import exchange
    /// resolves the segment by it, so it travels with the descriptor as-is
    pub token_id: u32,
}

/* ============================== Global init ============================== */

/// Shared inner so that contexts derived from a guard keep the init alive.
/// Held via `Rc`, which keeps the guard (and everything holding it)
/// `!Send`/`!Sync`.
struct UrmaInner;

impl Drop for UrmaInner {
    fn drop(&mut self) {
        unsafe { ffi::urma_uninit() };
    }
}

/// Global init guard: maps the first `Urma::init()`..last-drop to
/// `urma_init`/`urma_uninit`. Later `init()` calls on the same thread clone the
/// cached guard (see [`Urma::init`]). Every [`Context`] created from a guard
/// keeps it alive internally, so `urma_uninit` always runs after the last
/// context is deleted, wherever the guard itself is dropped.
pub struct Urma {
    inner: Rc<UrmaInner>,
}

impl Urma {
    /// `urma_init` may only run once per process: the C library returns
    /// URMA_EEXIST on a second call. To keep the guard ergonomic (and role-
    /// separated resource sets, as in the examples, working), a successful
    /// initialization is cached per thread and later calls clone it;
    /// `urma_uninit` runs once, when the thread's initialization goes away.
    /// Per-thread caching matches the crate's design: resources are !Send and
    /// stay on the thread that created them.
    pub fn init() -> Result<Self> {
        thread_local! {
            static INIT: RefCell<Option<Rc<UrmaInner>>> = const { RefCell::new(None) };
        }
        INIT.with(|slot| {
            if let Some(inner) = slot.borrow().clone() {
                return Ok(Urma { inner });
            }
            let mut attr = ffi::urma_init_attr_t {
                token: 0,
                uasid: 0, /* 0 means assigned by the system */
            };
            check_status(unsafe { ffi::urma_init(&mut attr) }, "urma_init")?;
            let inner = Rc::new(UrmaInner);
            *slot.borrow_mut() = Some(Rc::clone(&inner));
            Ok(Urma { inner })
        })
    }
}

/* ============================== Device enumeration ============================== */

/// Enumerate local URMA device names (`urma_get_device_list`). Returns an
/// empty list when no device exists, Err when `urma_init` fails.
pub fn list_devices() -> Result<Vec<String>> {
    let _urma = Urma::init()?;
    let mut cnt: i32 = 0;
    let list = unsafe { ffi::urma_get_device_list(&mut cnt) };
    if list.is_null() || cnt <= 0 {
        if !list.is_null() {
            unsafe { ffi::urma_free_device_list(list) };
        }
        return Ok(Vec::new());
    }
    let mut names = Vec::with_capacity(cnt as usize);
    for i in 0..cnt as usize {
        let dev = unsafe { *list.add(i) };
        if !dev.is_null() {
            /* the driver guarantees name is NUL-terminated */
            let name = unsafe { std::ffi::CStr::from_ptr((*dev).name.as_ptr()) };
            names.push(name.to_string_lossy().into_owned());
        }
    }
    unsafe { ffi::urma_free_device_list(list) };
    Ok(names)
}

/* ============================== context ============================== */

struct ContextInner {
    raw: NonNull<ffi::urma_context_t>,
    /// keeps the global init alive until the last context is deleted
    _urma: Rc<UrmaInner>,
}

impl Drop for ContextInner {
    fn drop(&mut self) {
        unsafe { ffi::urma_delete_context(self.raw.as_ptr()) };
    }
}

/// Context binding a device + EID; container for all resource operations.
/// A cheap cloneable handle: the underlying `urma_context_t` is deleted when
/// the last handle is dropped. Every resource created from a context holds its
/// own handle, so the context always outlives its children.
#[derive(Clone)]
pub struct Context {
    inner: Rc<ContextInner>,
    eid: Eid,
}

impl Context {
    /// Create by device name: get device -> enumerate EIDs, take the first ->
    /// `urma_create_context`. The `Urma` guard argument proves the library is
    /// initialized (and is kept alive internally).
    pub fn create(urma: &Urma, dev_name: &str) -> Result<Self> {
        let cname = CString::new(dev_name)
            .map_err(|_| Error::Invalid("device name contains NUL".into()))?;
        let dev = unsafe { ffi::urma_get_device_by_name(cname.as_ptr() as *mut _) };
        if dev.is_null() {
            return Err(Error::NotFound(format!(
                "device '{dev_name}' not found, see list_devices()"
            )));
        }

        let mut eid_cnt: u32 = 0;
        let list = unsafe { ffi::urma_get_eid_list(dev, &mut eid_cnt) };
        if list.is_null() || eid_cnt == 0 {
            if !list.is_null() {
                unsafe { ffi::urma_free_eid_list(list) };
            }
            return Err(Error::NotFound(format!("no eid on device '{dev_name}'")));
        }
        let info = unsafe { *list };
        let eid = Eid(info.eid.raw);
        unsafe { ffi::urma_free_eid_list(list) };

        let raw = unsafe { ffi::urma_create_context(dev, info.eid_index) };
        let raw = NonNull::new(raw).ok_or_else(|| null_err("urma_create_context"))?;
        Ok(Context {
            inner: Rc::new(ContextInner { raw, _urma: Rc::clone(&urma.inner) }),
            eid,
        })
    }

    pub fn eid(&self) -> Eid {
        self.eid
    }

    fn raw(&self) -> *mut ffi::urma_context_t {
        self.inner.raw.as_ptr()
    }
}

/* ============================== Completion queue ============================== */

/// Safe view of one completion record (cr)
#[derive(Clone, Copy, Debug)]
pub struct Completion {
    pub status: i32,
    pub user_ctx: u64,
    pub completion_len: u32,
}

impl Completion {
    /// Whether this completion record reports success (`URMA_CR_SUCCESS`)
    pub fn is_success(&self) -> bool {
        self.status == ffi::URMA_CR_SUCCESS
    }
}

struct CqInner {
    jfce: NonNull<ffi::urma_jfce_t>,
    jfc: NonNull<ffi::urma_jfc_t>,
    /// the context is deleted only after this CQ is gone
    _ctx: Context,
}

impl Drop for CqInner {
    fn drop(&mut self) {
        unsafe {
            ffi::urma_delete_jfc(self.jfc.as_ptr());
            ffi::urma_delete_jfce(self.jfce.as_ptr());
        }
    }
}

/// jfce + jfc: all completion records are polled from here. Cloneable handle
/// with the same sharing semantics as [`Context`].
#[derive(Clone)]
pub struct CompletionQueue {
    inner: Rc<CqInner>,
}

impl CompletionQueue {
    pub fn new(ctx: &Context, depth: u32) -> Result<Self> {
        let jfce = unsafe { ffi::urma_create_jfce(ctx.raw()) };
        let jfce = NonNull::new(jfce).ok_or_else(|| null_err("urma_create_jfce"))?;
        let mut cfg = ffi::urma_jfc_cfg_t {
            depth,
            jfce: jfce.as_ptr(), /* jfce is a required field in urma_jfc_cfg_t */
            ..Default::default()
        };
        let jfc = unsafe { ffi::urma_create_jfc(ctx.raw(), &mut cfg) };
        let Some(jfc) = NonNull::new(jfc) else {
            unsafe { ffi::urma_delete_jfce(jfce.as_ptr()) };
            return Err(null_err("urma_create_jfc"));
        };
        Ok(CompletionQueue { inner: Rc::new(CqInner { jfce, jfc, _ctx: ctx.clone() }) })
    }

    fn jfc(&self) -> *mut ffi::urma_jfc_t {
        self.inner.jfc.as_ptr()
    }

    /// Non-blocking single poll; `Ok(None)` means no completion record yet
    pub fn poll(&self) -> Result<Option<Completion>> {
        let mut cr = ffi::urma_cr_t::default();
        let n = unsafe { ffi::urma_poll_jfc(self.jfc(), 1, &mut cr) };
        if n < 0 {
            return Err(Error::Status(n, "urma_poll_jfc"));
        }
        if n == 0 {
            return Ok(None);
        }
        Ok(Some(Completion {
            status: cr.status,
            user_ctx: cr.user_ctx,
            completion_len: cr.completion_len,
        }))
    }

    /// Poll until the success completion record for the given user_ctx arrives
    pub fn wait_read(&self, expected_ctx: u64) -> Result<Completion> {
        for _ in 0..POLL_RETRIES {
            if let Some(cr) = self.poll()? {
                if !cr.is_success() || cr.user_ctx != expected_ctx {
                    return Err(Error::BadCompletion { status: cr.status, user_ctx: cr.user_ctx });
                }
                return Ok(cr);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        Err(Error::PollTimeout { user_ctx: expected_ctx })
    }
}

/* ============================== jetty ============================== */

/// jetty creation parameters
#[derive(Clone, Copy, Debug)]
pub struct JettyOpts {
    pub depth: u32,
    /// Must be true for bonding devices + RM mode
    pub multi_path: bool,
    /// Plain token value (`urma_token_t`), matched by the peer on import
    pub token_value: u32,
    /// Max local sges per work request (scatter/gather list length), applied to
    /// both the jfs and the shared jfr; the driver validates it against the
    /// device capabilities (`max_jfs_sge` / `max_jfr_sge`, typically 13+ / 4+).
    /// 1 (default) disables scatter/gather.
    pub max_sge: u8,
}

impl Default for JettyOpts {
    fn default() -> Self {
        JettyOpts { depth: DEFAULT_DEPTH, multi_path: false, token_value: TOKEN_VALUE, max_sge: 1 }
    }
}

/// jfr + jetty (communication endpoint created in shared-jfr mode)
pub struct Jetty {
    jfr: NonNull<ffi::urma_jfr_t>,
    raw: NonNull<ffi::urma_jetty_t>,
    id: JettyId,
    max_sge: u8,
    /// keeps the CQ (and through it the context) alive until jfr/jetty are deleted
    _cq: CompletionQueue,
}

impl Jetty {
    /// Fixed RM transport mode (order_type uses default 0); the tp_type
    /// (CTP) is only chosen later, at import time — see Peer::import
    pub fn new(ctx: &Context, cq: &CompletionQueue, opts: JettyOpts) -> Result<Self> {
        /* jfr: one-sided READ never receives data through it, but jetty is created with share_jfr, so it must be created first */
        let mut jfr_cfg = ffi::urma_jfr_cfg_t {
            depth: opts.depth,
            flag: ffi::urma_jfr_flag_t { value: 0 }, /* NO_TAG_MATCHING + order 0 */
            trans_mode: ffi::URMA_TM_RM,
            max_sge: opts.max_sge,
            min_rnr_timer: ffi::URMA_TYPICAL_MIN_RNR_TIMER,
            jfc: cq.jfc(),
            token_value: ffi::urma_token_t { token: opts.token_value },
            ..Default::default() /* id = 0 means assigned by the system */
        };
        let jfr = unsafe { ffi::urma_create_jfr(ctx.raw(), &mut jfr_cfg) };
        let jfr = NonNull::new(jfr).ok_or_else(|| null_err("urma_create_jfr"))?;

        let jfs_flag = ffi::urma_jfs_flag_t::default()
            .with_order_type(0)
            .with_multi_path(opts.multi_path);

        let jfs_cfg = ffi::urma_jfs_cfg_t {
            depth: opts.depth,
            flag: jfs_flag,
            trans_mode: ffi::URMA_TM_RM,
            priority: ffi::URMA_MAX_PRIORITY,
            max_sge: opts.max_sge,
            rnr_retry: ffi::URMA_TYPICAL_RNR_RETRY,
            err_timeout: ffi::URMA_TYPICAL_ERR_TIMEOUT,
            jfc: cq.jfc(),
            ..Default::default()
        };

        let mut jetty_cfg = ffi::urma_jetty_cfg_t {
            flag: ffi::urma_jetty_flag_t { value: ffi::URMA_JETTY_FLAG_SHARE_JFR },
            jfs_cfg,
            recv: ffi::urma_jetty_recv_cfg_t { jfr: jfr.as_ptr(), jfc: std::ptr::null_mut() },
            ..Default::default()
        };

        let raw = unsafe { ffi::urma_create_jetty(ctx.raw(), &mut jetty_cfg) };
        let Some(raw) = NonNull::new(raw) else {
            let e = null_err("urma_create_jetty");
            unsafe { ffi::urma_delete_jfr(jfr.as_ptr()) };
            return Err(e);
        };
        let id = JettyId::from_raw(unsafe { &raw.as_ref().jetty_id });
        Ok(Jetty { jfr, raw, id, max_sge: opts.max_sge, _cq: cq.clone() })
    }

    pub fn id(&self) -> JettyId {
        self.id
    }

    /// Export the remote-import descriptor as an opaque blob
    /// (`urma_get_rjetty`; requires a shared jfr, which [`Jetty::new`] always
    /// sets). Same bonding rationale as [`RegisteredSeg::export_seg_ctx`]:
    /// the appended ext lets the importer skip the kernel-side jetty exchange.
    pub fn export_rjetty(&self) -> Result<Vec<u8>> {
        let mut raw: *mut ffi::urma_rjetty_t = std::ptr::null_mut();
        let mut len: u32 = 0;
        check_status(
            unsafe { ffi::urma_get_rjetty(self.raw.as_ptr(), &mut raw, &mut len) },
            "urma_get_rjetty",
        )?;
        let Some(raw) = NonNull::new(raw) else {
            return Err(null_err("urma_get_rjetty"));
        };
        let min = std::mem::size_of::<ffi::urma_rjetty_t>();
        if (len as usize) < min {
            unsafe { ffi::urma_put_rjetty(raw.as_ptr()) };
            return Err(Error::Invalid(format!("rjetty blob too short: {len} < {min}")));
        }
        let blob = unsafe { std::slice::from_raw_parts(raw.as_ptr() as *const u8, len as usize) };
        let blob = blob.to_vec();
        unsafe { ffi::urma_put_rjetty(raw.as_ptr()) };
        Ok(blob)
    }

    /// Validate a local sge list against `JettyOpts::max_sge` and convert to ffi
    fn check_local(&self, local: &[LocalSge]) -> Result<Vec<ffi::urma_sge_t>> {
        if local.is_empty() {
            return Err(Error::Invalid("empty local sge list".into()));
        }
        if local.len() > usize::from(self.max_sge) {
            return Err(Error::Invalid(format!(
                "{} local sges exceed max_sge {} (see JettyOpts::max_sge)",
                local.len(),
                self.max_sge
            )));
        }
        Ok(local.iter().map(|s| s.to_ffi()).collect())
    }

    /// Post one one-sided READ: the contiguous remote range at `remote_va` ->
    /// the local sge list. src is the remote address (the API allows exactly
    /// one src sge, hence a single remote va), dst the local address; multiple
    /// local sges give scatter. Wait for the user_ctx completion via
    /// [`CompletionQueue::wait_read`].
    pub fn post_read(
        &self,
        peer: &Peer,
        remote_va: u64,
        local: &[LocalSge],
        user_ctx: u64,
    ) -> Result<()> {
        let mut local_sges = self.check_local(local)?;
        let total = local
            .iter()
            .try_fold(0u32, |a, s| a.checked_add(s.len))
            .ok_or_else(|| Error::Invalid("total local sge length overflows u32".into()))?;
        let mut remote_sge = ffi::urma_sge_t {
            addr: remote_va,
            len: total,
            tseg: peer.tseg.as_ptr(),
            ..Default::default()
        };
        let remote_sg = ffi::urma_sg_t { sge: &mut remote_sge, num_sge: 1 };
        let local_sg =
            ffi::urma_sg_t { sge: local_sges.as_mut_ptr(), num_sge: local_sges.len() as u32 };
        let rw = ffi::urma_rw_wr_t { src: remote_sg, dst: local_sg, ..Default::default() };

        let mut wr = ffi::urma_jfs_wr_t {
            opcode: ffi::URMA_OPC_READ,
            flag: ffi::urma_jfs_wr_flag_t { value: ffi::URMA_JFS_WR_FLAG_COMPLETE_ENABLE },
            tjetty: peer.tjetty.as_ptr(),
            user_ctx,
            rw,
            ..Default::default()
        };

        let mut bad_wr: *mut ffi::urma_jfs_wr_t = std::ptr::null_mut();
        check_status(
            unsafe { ffi::urma_post_jetty_send_wr(self.raw.as_ptr(), &mut wr, &mut bad_wr) },
            "urma_post_jetty_send_wr(READ)",
        )
    }

    /// Post one two-sided SEND: send the local sge list (gather) to `peer`'s
    /// receive buffer (posted beforehand via [`Jetty::post_recv`]). Both sides
    /// get a completion record: local send completion means "data consumed by
    /// the peer", peer recv completion means "data landed".
    /// A plain SEND is modeled via rw.src: `urma_send_wr_t.src` shares union
    /// offset 0 with `urma_rw_wr_t.src`, and the other union members stay 0.
    pub fn post_send(&self, peer: &Peer, local: &[LocalSge], user_ctx: u64) -> Result<()> {
        let mut sges = self.check_local(local)?;
        let sg = ffi::urma_sg_t { sge: sges.as_mut_ptr(), num_sge: sges.len() as u32 };
        /* plain SEND reuses rw.src (union offset 0); other union members stay 0 */
        let rw = ffi::urma_rw_wr_t { src: sg, ..Default::default() };

        let mut wr = ffi::urma_jfs_wr_t {
            opcode: ffi::URMA_OPC_SEND,
            flag: ffi::urma_jfs_wr_flag_t { value: ffi::URMA_JFS_WR_FLAG_COMPLETE_ENABLE },
            tjetty: peer.tjetty.as_ptr(),
            user_ctx,
            rw,
            ..Default::default()
        };

        let mut bad_wr: *mut ffi::urma_jfs_wr_t = std::ptr::null_mut();
        check_status(
            unsafe { ffi::urma_post_jetty_send_wr(self.raw.as_ptr(), &mut wr, &mut bad_wr) },
            "urma_post_jetty_send_wr(SEND)",
        )
    }

    /// Post a receive buffer to the JFR (two-sided receive direction): data
    /// from the peer's SEND lands in the local sge list (scatter), and on
    /// arrival the local CQ gets a `user_ctx` completion record
    /// (completion_len is the arrived length). Must be posted before data
    /// arrives (buffer first, then let the peer send), otherwise RNR is
    /// triggered.
    pub fn post_recv(&self, local: &[LocalSge], user_ctx: u64) -> Result<()> {
        let mut sges = self.check_local(local)?;
        let sg = ffi::urma_sg_t { sge: sges.as_mut_ptr(), num_sge: sges.len() as u32 };

        let mut wr = ffi::urma_jfr_wr_t { src: sg, user_ctx, ..Default::default() };

        let mut bad_wr: *mut ffi::urma_jfr_wr_t = std::ptr::null_mut();
        check_status(
            unsafe { ffi::urma_post_jfr_wr(self.jfr.as_ptr(), &mut wr, &mut bad_wr) },
            "urma_post_jfr_wr",
        )
    }
}

impl Drop for Jetty {
    fn drop(&mut self) {
        unsafe {
            ffi::urma_delete_jetty(self.raw.as_ptr());
            ffi::urma_delete_jfr(self.jfr.as_ptr());
        }
    }
}

/* ============================== Memory registration ============================== */

/// Registered local memory, the low-level addressing unit: a target segment
/// plus its address window. The type parameter says who owns the registered
/// memory: `RegisteredSeg` (short for `RegisteredSeg<()>`) borrows memory
/// owned elsewhere (an external allocation, hugepages) and the caller must
/// keep it alive; [`RegisteredBuf`] owns a page-aligned buffer that is freed
/// only after the segment is unregistered.
pub struct RegisteredSeg<B = ()> {
    tseg: NonNull<ffi::urma_target_seg_t>,
    pub va: u64,
    pub len: u64,
    /// keeps the context alive until the segment is unregistered
    _ctx: Context,
    /// the registered memory itself, or `()` when it is owned elsewhere
    buf: B,
}

impl RegisteredSeg<()> {
    /// Grant the peer READ|WRITE; no user token id is requested:
    /// token_policy/cacheable/token_id_valid are all 0, only the access field
    /// is set, and authentication relies on the plain `token_value` alone.
    /// (The driver still allocates an internal token id at register —
    /// `descriptor()` must ship it for the peer's import to resolve.)
    pub fn register(ctx: &Context, ptr: *mut u8, len: usize, token_value: u32) -> Result<Self> {
        Self::register_in(ctx, ptr, len, token_value, ())
    }
}

impl<B> RegisteredSeg<B> {
    /// Shared registration path: register `[ptr, ptr + len)` and adopt `buf`
    /// as the memory owner (`()` when the memory is owned elsewhere)
    fn register_in(
        ctx: &Context,
        ptr: *mut u8,
        len: usize,
        token_value: u32,
        buf: B,
    ) -> Result<Self> {
        let flag = ffi::urma_reg_seg_flag_t::default()
            .with_access(ffi::URMA_ACCESS_READ | ffi::URMA_ACCESS_WRITE);

        let mut cfg = ffi::urma_seg_cfg_t {
            va: ptr as u64,
            len: len as u64,
            token_value: ffi::urma_token_t { token: token_value },
            flag,
            ..Default::default()
        };

        let tseg = unsafe { ffi::urma_register_seg(ctx.raw(), &mut cfg) };
        let tseg = NonNull::new(tseg).ok_or_else(|| null_err("urma_register_seg"))?;
        Ok(RegisteredSeg { tseg, va: ptr as u64, len: len as u64, _ctx: ctx.clone(), buf })
    }

    /// Public descriptor: published to the directory for the peer to import
    pub fn descriptor(&self) -> SegDesc {
        let seg = unsafe { self.tseg.as_ref().seg };
        SegDesc {
            eid: Eid(seg.ubva.eid),
            uasid: seg.ubva.uasid,
            va: seg.ubva.va(),
            // has_user_info (bit 14) would promise extension data appended
            // after urma_seg_t; a descriptor crossing the wire as plain values
            // never carries that payload, so the bit must not travel (the
            // bonding provider would parse nonexistent trailing bytes).
            attr: seg.attr.value & !(1 << 14),
            len: seg.len,
            // token_id is allocated by the driver at register (a kernel
            // bitmap id on bonding, 0 only for the first-ever registration)
            // and is the key of the import exchange: the importer's kernel
            // sends it back and the REMOTE looks the segment up by it
            // (ubagg_connect.c handle_seg_req). It must travel as-is, exactly
            // like the official sample ships tseg->seg.token_id.
            token_id: seg.token_id,
        }
    }

    /// Export the full segment context as an opaque blob
    /// (`urma_get_seg_ctx`; the library-allocated buffer is copied out and
    /// freed with `urma_put_seg_ctx`). On bonding devices the blob appends
    /// the per-physical-device info (has_user_info ext: each pseg's EID +
    /// token_id), so the importer resolves everything locally and skips the
    /// kernel-side seg exchange (ubagg_connect_xchg_seg) that rides the
    /// management comm channel — the path urma_perftest uses on bonding.
    /// On plain devices the blob is just the `urma_seg_t`.
    pub fn export_seg_ctx(&self) -> Result<Vec<u8>> {
        let mut raw: *mut ffi::urma_seg_t = std::ptr::null_mut();
        let mut size: u32 = 0;
        check_status(
            unsafe { ffi::urma_get_seg_ctx(self.tseg.as_ptr(), &mut raw, &mut size) },
            "urma_get_seg_ctx",
        )?;
        let Some(raw) = NonNull::new(raw) else {
            return Err(null_err("urma_get_seg_ctx"));
        };
        let min = std::mem::size_of::<ffi::urma_seg_t>();
        if (size as usize) < min {
            unsafe { ffi::urma_put_seg_ctx(raw.as_ptr()) };
            return Err(Error::Invalid(format!("seg ctx blob too short: {size} < {min}")));
        }
        let blob =
            unsafe { std::slice::from_raw_parts(raw.as_ptr() as *const u8, size as usize) };
        let blob = blob.to_vec();
        unsafe { ffi::urma_put_seg_ctx(raw.as_ptr()) };
        Ok(blob)
    }

    /// Window `[off, off + len)` of this segment as a [`LocalSge`],
    /// bounds-checked; shorthand for [`LocalSge::new`]
    pub fn sge(&self, off: usize, len: u32) -> Result<LocalSge<'_>> {
        LocalSge::new(self, off, len)
    }
}

impl<B> Drop for RegisteredSeg<B> {
    fn drop(&mut self) {
        unsafe { ffi::urma_unregister_seg(self.tseg.as_ptr()) };
        /* an owned buffer is freed after this body, so the memory always outlives its registration */
    }
}

/* ============================== Import peer resources ============================== */

/// Imported peer resources (segment + jetty); handles required to issue a
/// one-sided READ
pub struct Peer {
    tseg: NonNull<ffi::urma_target_seg_t>,
    tjetty: NonNull<ffi::urma_target_jetty_t>,
    /// keeps the context alive until the peer resources are unimported
    _ctx: Context,
}

impl Peer {
    /// Copy a blob into a u64-aligned buffer: a C struct is read through the
    /// resulting pointer, while a plain `Vec<u8>` guarantees no alignment
    fn aligned_blob(bytes: &[u8], min: usize, what: &str) -> Result<Vec<u64>> {
        if bytes.len() < min {
            return Err(Error::Invalid(format!(
                "{what} blob too short: {} < {min}",
                bytes.len()
            )));
        }
        let mut buf = vec![0u64; bytes.len().div_ceil(8)];
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                buf.as_mut_ptr() as *mut u8,
                bytes.len(),
            )
        };
        Ok(buf)
    }

    /// Import from the exported blobs ([`RegisteredSeg::export_seg_ctx`] /
    /// [`Jetty::export_rjetty`]). Preferred over [`Peer::import`] on bonding
    /// devices: the blobs carry the peer's per-physical-device info, so the
    /// provider resolves psegs/pjettys locally from its topology snapshot
    /// instead of doing the kernel-side info exchange — which rides the
    /// management comm channel and fails with errno ENOEXEC when that channel
    /// is down. `tp_type` is the importer's choice and is patched to CTP in
    /// the rjetty blob, exactly like [`Peer::import`] sets it.
    pub fn import_ctx(
        ctx: &Context,
        seg_ctx: &[u8],
        rjetty: &[u8],
        token_value: u32,
    ) -> Result<Self> {
        let seg = Self::aligned_blob(seg_ctx, std::mem::size_of::<ffi::urma_seg_t>(), "seg ctx")?;
        let mut rj =
            Self::aligned_blob(rjetty, std::mem::size_of::<ffi::urma_rjetty_t>(), "rjetty")?;
        let rj_ptr = rj.as_mut_ptr() as *mut ffi::urma_rjetty_t;
        unsafe { (*rj_ptr).tp_type = ffi::URMA_TP_CTP }; /* CTP-RM, chosen at import */

        /* cacheable/mapping are both 0 (NON_CACHEABLE + SEG_NOMAP); only the access field is set */
        let imp_flag = ffi::urma_import_seg_flag_t::default()
            .with_access(ffi::URMA_ACCESS_READ | ffi::URMA_ACCESS_WRITE);

        let mut token = ffi::urma_token_t { token: token_value };
        let tseg = unsafe {
            ffi::urma_import_seg(
                ctx.raw(),
                seg.as_ptr() as *mut ffi::urma_seg_t,
                &mut token,
                0,
                imp_flag,
            )
        };
        let tseg = NonNull::new(tseg).ok_or_else(|| null_err("urma_import_seg"))?;

        let tjetty = unsafe { ffi::urma_import_jetty(ctx.raw(), rj_ptr, &mut token) };
        let Some(tjetty) = NonNull::new(tjetty) else {
            let e = null_err("urma_import_jetty");
            unsafe { ffi::urma_unimport_seg(tseg.as_ptr()) };
            return Err(e);
        };
        Ok(Peer { tseg, tjetty, _ctx: ctx.clone() })
    }

    pub fn import(ctx: &Context, seg: SegDesc, jetty: JettyId, token_value: u32) -> Result<Self> {
        let mut ubva = ffi::urma_ubva_t { eid: seg.eid.0, uasid: seg.uasid, ..Default::default() };
        ubva.set_va(seg.va);
        let mut seg_in = ffi::urma_seg_t {
            ubva,
            len: seg.len,
            attr: ffi::urma_seg_attr_t { value: seg.attr },
            token_id: seg.token_id,
        };

        /* cacheable/mapping are both 0 (NON_CACHEABLE + SEG_NOMAP); only the access field is set */
        let imp_flag = ffi::urma_import_seg_flag_t::default()
            .with_access(ffi::URMA_ACCESS_READ | ffi::URMA_ACCESS_WRITE);

        let mut token = ffi::urma_token_t { token: token_value };
        let tseg = unsafe {
            ffi::urma_import_seg(ctx.raw(), &mut seg_in, &mut token, 0, imp_flag)
        };
        let tseg = NonNull::new(tseg).ok_or_else(|| null_err("urma_import_seg"))?;

        let mut rjetty = ffi::urma_rjetty_t {
            jetty_id: jetty.to_raw(),
            trans_mode: ffi::URMA_TM_RM,
            policy: ffi::URMA_JETTY_GRP_POLICY_RR,
            type_: ffi::URMA_TARGET_JETTY,
            tp_type: ffi::URMA_TP_CTP, /* CTP-RM; tp_type is chosen at import, not at create */
            ..Default::default()
        };

        let tjetty =
            unsafe { ffi::urma_import_jetty(ctx.raw(), &mut rjetty, &mut token) };
        let Some(tjetty) = NonNull::new(tjetty) else {
            let e = null_err("urma_import_jetty");
            unsafe { ffi::urma_unimport_seg(tseg.as_ptr()) };
            return Err(e);
        };
        Ok(Peer { tseg, tjetty, _ctx: ctx.clone() })
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        unsafe {
            ffi::urma_unimport_jetty(self.tjetty.as_ptr());
            ffi::urma_unimport_seg(self.tseg.as_ptr());
        }
    }
}

/* ============================== Page-aligned buffer ============================== */

/// 4KB-aligned heap buffer (uses the std allocator, no libc crate needed).
/// Plain `urma_register_seg` documents no alignment requirement; page
/// alignment is kept as cheap insurance (use [`PAGE_SIZE`], never a magic
/// 4096).
pub struct PageBuf {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl PageBuf {
    pub fn new(len: usize) -> Result<Self> {
        if len == 0 {
            /* a zero-sized layout is UB for the global allocator; a zero-length buffer is useless anyway */
            return Err(Error::Invalid("zero-length page buffer".into()));
        }
        let layout = Layout::from_size_align(len, PAGE_SIZE)
            .map_err(|e| Error::Invalid(format!("bad layout: {e}")))?;
        let ptr = unsafe { alloc_zeroed(layout) };
        let ptr =
            NonNull::new(ptr).ok_or_else(|| Error::Invalid("alloc page buffer failed".into()))?;
        Ok(PageBuf { ptr, layout })
    }
}

impl std::ops::Deref for PageBuf {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl std::ops::DerefMut for PageBuf {
    fn deref_mut(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.layout.size()) }
    }
}

impl Drop for PageBuf {
    fn drop(&mut self) {
        unsafe { dealloc(self.ptr.as_ptr(), self.layout) };
    }
}

/* ============================== Registered buffer ============================== */

/// Page-aligned buffer plus its registration as one unit (alias for
/// `RegisteredSeg<PageBuf>`): the buffer is owned by the registration and
/// freed only after the segment is unregistered (see the `Drop` impl), so
/// memory and registration can never get out of sync. Addressed in the post_*
/// methods via [`RegisteredSeg::sge`]; usable as a plain byte slice.
pub type RegisteredBuf = RegisteredSeg<PageBuf>;

impl RegisteredSeg<PageBuf> {
    /// Allocate `len` page-aligned bytes and register them (READ|WRITE for
    /// the peer), see [`RegisteredSeg::register`]
    pub fn new(ctx: &Context, len: usize, token_value: u32) -> Result<Self> {
        let mut buf = PageBuf::new(len)?;
        Self::register_in(ctx, buf.as_mut_ptr(), len, token_value, buf)
    }
}

impl std::ops::Deref for RegisteredSeg<PageBuf> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf
    }
}

impl std::ops::DerefMut for RegisteredSeg<PageBuf> {
    fn deref_mut(&mut self) -> &mut [u8] {
        &mut self.buf
    }
}

/* ============================== Local scatter/gather element ============================== */

/// One local scatter/gather element for the post_* methods: a window inside a
/// registered segment. It anchors a shared borrow of the segment it was cut
/// from, so it cannot outlive its memory, and the address always comes from
/// the same object as the tseg, so a mismatched pointer/segment pair cannot
/// be expressed. The post_* methods take a `&[LocalSge]` — one element is the
/// common case, several give scatter/gather (bounded by [`JettyOpts::max_sge`]).
#[derive(Clone, Copy)]
pub struct LocalSge<'a> {
    tseg: NonNull<ffi::urma_target_seg_t>,
    addr: u64,
    len: u32,
    /// anchors the borrow of the segment this window was cut from
    _seg: PhantomData<&'a RegisteredSeg>,
}

impl<'a> LocalSge<'a> {
    /// Window `[off, off + len)` of a registered segment, bounds-checked.
    /// Works with any [`RegisteredSeg`], including memory not owned by a
    /// [`RegisteredBuf`] (external allocations, hugepages); for the common
    /// case prefer [`RegisteredSeg::sge`].
    pub fn new<B>(seg: &'a RegisteredSeg<B>, off: usize, len: u32) -> Result<Self> {
        let in_range =
            off.checked_add(len as usize).is_some_and(|end| (end as u64) <= seg.len);
        if !in_range {
            return Err(Error::Invalid(format!(
                "local range [{off}, +{len}) exceeds segment (len {})",
                seg.len
            )));
        }
        Ok(LocalSge { tseg: seg.tseg, addr: seg.va + off as u64, len, _seg: PhantomData })
    }

    fn to_ffi(self) -> ffi::urma_sge_t {
        ffi::urma_sge_t {
            addr: self.addr,
            len: self.len,
            tseg: self.tseg.as_ptr(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// urma_init returns URMA_EEXIST on a second call, so Urma::init must cache
    /// and clone instead. No device is needed: init only loads providers
    /// (device errors would come later, from Context::create).
    #[test]
    fn init_is_idempotent_per_thread() {
        let _first = Urma::init().expect("first init");
        let _second = Urma::init().expect("second init must clone the guard, not hit URMA_EEXIST");
    }
}
