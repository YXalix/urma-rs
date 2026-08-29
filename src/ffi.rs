//! Raw FFI bindings for URMA (hand-written, unsafe).
//!
//! Transcribed from the vendored headers under `include/`; only the subset
//! used by the safe layer is declared. Modeling notes:
//! - Objects with internal fields (context / jfc / jfr / jfce / jetty /
//!   device) are opaque types that cross the API boundary only as pointers;
//!   `urma_jetty_t` models only the documented prefix fields.
//! - C bitfield unions (`urma_*_flag_t`) are modeled as `value: u32` plus
//!   named constants and shift-wrapping helper methods.
//! - Anonymous unions are modeled by their largest member (`urma_jetty_cfg_t`'s
//!   recv union; the payload union of `urma_jfs_wr_t` by `urma_rw_wr_t`).
//! - `urma_ubva_t` is packed; its unaligned `va` field goes through accessors.
//!
//! The ABI is guarded by the layout unit tests at the end of the file.
//! Linking: `#[link(name = "urma")]` links the system library liburma.so.

#![allow(non_camel_case_types, non_snake_case, dead_code, clippy::missing_safety_doc)]

use std::os::raw::{c_char, c_int};

/* ============================== Constants (urma_opcode.h) ============================== */

pub const URMA_EID_SIZE: usize = 16;
pub const URMA_MAX_NAME: usize = 64;
pub const URMA_MAX_PATH: usize = 4096;
pub const URMA_GUID_SIZE: usize = 16;
pub const URMA_MAX_PRIORITY_CNT: usize = 16;
pub const MAX_PORT_CNT: usize = 8;

/// urma_transport_type_t (values of urma_device_t.type_)
pub const URMA_TRANSPORT_INVALID: c_int = -1;
pub const URMA_TRANSPORT_UB: c_int = 0;
pub const URMA_TRANSPORT_IB: c_int = 1;
pub const URMA_TRANSPORT_IP: c_int = 2;
pub const URMA_TRANSPORT_SOFTUB: c_int = 3;
pub const URMA_TRANSPORT_HNS_UB: c_int = 5;

/// urma_vlog_level_t (urma_types.h)
pub const URMA_VLOG_LEVEL_EMERG: i32 = 0;
pub const URMA_VLOG_LEVEL_ALERT: i32 = 1;
pub const URMA_VLOG_LEVEL_CRIT: i32 = 2;
pub const URMA_VLOG_LEVEL_ERR: i32 = 3;
pub const URMA_VLOG_LEVEL_WARNING: i32 = 4;
pub const URMA_VLOG_LEVEL_NOTICE: i32 = 5;
pub const URMA_VLOG_LEVEL_INFO: i32 = 6;
pub const URMA_VLOG_LEVEL_DEBUG: i32 = 7;

/// C: `typedef void (*urma_log_cb_t)(int level, char *message)` — the message
/// arrives pre-formatted (the library owns it; only valid during the call)
pub type urma_log_cb_t = Option<unsafe extern "C" fn(level: i32, message: *mut c_char)>;

/// token validation policy (values of the token_policy field of
/// urma_reg_seg_flag_t / urma_jfr_flag_t)
pub const URMA_TOKEN_NONE: u32 = 0;
pub const URMA_TOKEN_PLAIN_TEXT: u32 = 1;
pub const URMA_TOKEN_SIGNED: u32 = 2;
pub const URMA_TOKEN_ALL_ENCRYPTED: u32 = 3;

pub const URMA_NON_CACHEABLE: u32 = 0;
pub const URMA_CACHEABLE: u32 = 1;

/// Memory segment access rights, composable (go into the access field of the
/// flag unions, see each type's helper methods)
pub const URMA_ACCESS_LOCAL_ONLY: u32 = 0x1 << 0;
pub const URMA_ACCESS_READ: u32 = 0x1 << 1;
pub const URMA_ACCESS_WRITE: u32 = 0x1 << 2;
pub const URMA_ACCESS_ATOMIC: u32 = 0x1 << 3;

pub const URMA_SEG_NOMAP: u32 = 0;
pub const URMA_SEG_MAPPED: u32 = 1;

/// urma_transport_mode_t
pub const URMA_TM_RM: u32 = 0x1; /* reliable message */
pub const URMA_TM_RC: u32 = 0x1 << 1; /* reliable connection */
pub const URMA_TM_UM: u32 = 0x1 << 2; /* unreliable message */

/// urma_tp_type_t
pub const URMA_TP_RTP: u32 = 0;
pub const URMA_TP_CTP: u32 = 1;
pub const URMA_TP_UTP: u32 = 2;

/// urma_target_type_t
pub const URMA_TARGET_JFR: u32 = 0;
pub const URMA_TARGET_JETTY: u32 = 1;
pub const URMA_TARGET_JETTY_GROUP: u32 = 2;

/// Bits of urma_device_feature_t (urma_types.h): oor 0, jfc_per_wr 1,
/// stride_op 2, load_store_op 3, non_pin 4, pmem 5, jfc_inline 6, spray_en 7,
/// selective_retrans 8, live_migrate 9, dca 10, jetty_grp 11, error_suspend 12,
/// outorder_comp 13, mn 14, clan 15, muti_seg_per_token_id 16, ipourma_en 17,
/// ctp_en 18, uboe 19
pub const URMA_FEATURE_CTP_EN: u32 = 1 << 18;

/// Bits of urma_tp_type_cap_t / urma_tp_type_en_t (same layout)
pub const URMA_TP_TYPE_CAP_RTP: u32 = 1 << 0;
pub const URMA_TP_TYPE_CAP_CTP: u32 = 1 << 1;
pub const URMA_TP_TYPE_CAP_UTP: u32 = 1 << 2;

/// Bits of urma_order_type_cap_t (bit index, NOT the urma_order_type_t value)
pub const URMA_ORDER_CAP_OT: u32 = 1 << 0;
pub const URMA_ORDER_CAP_OI: u32 = 1 << 1;
pub const URMA_ORDER_CAP_OL: u32 = 1 << 2;
pub const URMA_ORDER_CAP_NO: u32 = 1 << 3;

/// Bits of urma_tp_feature_t
pub const URMA_TP_FEAT_RM_MULTI_PATH: u32 = 1 << 0;
pub const URMA_TP_FEAT_RC_MULTI_PATH: u32 = 1 << 1;

/// urma_jetty_grp_policy_t
pub const URMA_JETTY_GRP_POLICY_RR: u32 = 0;
pub const URMA_JETTY_GRP_POLICY_HASH_HINT: u32 = 1;

/// urma_opcode_t (only the common ones are listed)
pub const URMA_OPC_WRITE: u32 = 0x00;
pub const URMA_OPC_WRITE_IMM: u32 = 0x01;
pub const URMA_OPC_READ: u32 = 0x10;
pub const URMA_OPC_SEND: u32 = 0x40;
pub const URMA_OPC_SEND_IMM: u32 = 0x41;

/// Bits of urma_jetty_flag_t
pub const URMA_JETTY_FLAG_SHARE_JFR: u32 = 1 << 0;

/// Bits of urma_jfs_flag_t: order_type spans bit3..11, multi_path is bit11
pub const URMA_JFS_FLAG_ORDER_TYPE_SHIFT: u32 = 3;
pub const URMA_JFS_FLAG_MULTI_PATH: u32 = 1 << 11;

/// Bits of urma_jfs_wr_flag_t: complete_enable is bit5
pub const URMA_JFS_WR_FLAG_COMPLETE_ENABLE: u32 = 1 << 5;

/// urma_status_t (function return value)
pub type urma_status_t = c_int;
pub const URMA_SUCCESS: urma_status_t = 0;
pub const URMA_EAGAIN: urma_status_t = 11;
pub const URMA_ENOMEM: urma_status_t = 12;
pub const URMA_ETIMEOUT: urma_status_t = 110;
pub const URMA_EINVAL: urma_status_t = 22;
pub const URMA_EEXIST: urma_status_t = 17;
pub const URMA_EINPROGRESS: urma_status_t = 115;
pub const URMA_FAIL: urma_status_t = 0x1000;

/// urma_cr_status_t (values of urma_cr_t.status)
pub const URMA_CR_SUCCESS: c_int = 0;
pub const URMA_CR_WR_FLUSH_ERR: c_int = 12;

pub const URMA_TYPICAL_RNR_RETRY: u8 = 7;
pub const URMA_TYPICAL_ERR_TIMEOUT: u8 = 17;
pub const URMA_TYPICAL_MIN_RNR_TIMER: u8 = 12;
pub const URMA_MAX_PRIORITY: u8 = 15;

/* ============================== Types (urma_types.h) ============================== */

/// C: `union urma_eid`. Modeled as raw bytes, keeping the union's alignment
/// (largest member u64).
#[repr(C, align(8))]
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct urma_eid_t {
    pub raw: [u8; URMA_EID_SIZE],
}

impl Default for urma_eid_t {
    fn default() -> Self {
        urma_eid_t { raw: [0; URMA_EID_SIZE] }
    }
}

impl std::fmt::Debug for urma_eid_t {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "urma_eid_t(")?;
        for i in (0..URMA_EID_SIZE).step_by(2) {
            if i > 0 {
                write!(f, ":")?;
            }
            write!(f, "{:02x}{:02x}", self.raw[i], self.raw[i + 1])?;
        }
        write!(f, ")")
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_init_attr_t {
    pub token: u64,
    pub uasid: u32, /* 0 means system-assigned */
}

/// C: `urma_device_t`. name/path are public fields (for device enumeration /
/// display); ops/sysfs_dev are provider-internal pointers, placeholder only,
/// never dereferenced.
#[repr(C)]
pub struct urma_device_t {
    pub name: [c_char; URMA_MAX_NAME],
    pub path: [c_char; URMA_MAX_PATH],
    pub type_: c_int, /* urma_transport_type_t */
    pub ops: *mut std::os::raw::c_void,
    pub sysfs_dev: *mut std::os::raw::c_void,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_eid_info_t {
    pub eid: urma_eid_t,
    pub eid_index: u32,
}

/* ---------- device capabilities (urma_query_device output) ---------- */

/// C: `union urma_device_feature_t`, modeled as `value` + helpers for the
/// bits the safe layer reads (see the URMA_FEATURE_* constants for the layout)
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_device_feature_t {
    pub value: u32,
}

impl urma_device_feature_t {
    /// CTP requires this device-level gate in addition to the per-mode
    /// tp cap bits (see docs/urma.md)
    pub fn ctp_en(&self) -> bool {
        self.value & URMA_FEATURE_CTP_EN != 0
    }

    pub fn with_ctp_en(mut self, on: bool) -> Self {
        if on {
            self.value |= URMA_FEATURE_CTP_EN;
        } else {
            self.value &= !URMA_FEATURE_CTP_EN;
        }
        self
    }
}

/// C: `union urma_atomic_feature_t` (cas bit0, swap bit1, fetch_and_* bits2..7)
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_atomic_feature_t {
    pub value: u32,
}

/// C: `union urma_tp_type_cap_t` — which tp types one trans mode supports
/// (same bit layout as `urma_tp_type_en_t`)
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_tp_type_cap_t {
    pub value: u32,
}

impl urma_tp_type_cap_t {
    pub fn rtp(&self) -> bool {
        self.value & URMA_TP_TYPE_CAP_RTP != 0
    }

    pub fn ctp(&self) -> bool {
        self.value & URMA_TP_TYPE_CAP_CTP != 0
    }

    pub fn utp(&self) -> bool {
        self.value & URMA_TP_TYPE_CAP_UTP != 0
    }
}

/// C: `union urma_order_type_cap_t` — which order types one trans mode
/// supports (bit index, not the urma_order_type_t value)
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_order_type_cap_t {
    pub value: u32,
}

impl urma_order_type_cap_t {
    pub fn ot(&self) -> bool {
        self.value & URMA_ORDER_CAP_OT != 0
    }

    pub fn oi(&self) -> bool {
        self.value & URMA_ORDER_CAP_OI != 0
    }

    pub fn ol(&self) -> bool {
        self.value & URMA_ORDER_CAP_OL != 0
    }

    pub fn no(&self) -> bool {
        self.value & URMA_ORDER_CAP_NO != 0
    }
}

/// C: `union urma_tp_feature_t` — per-mode multi-path support
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_tp_feature_t {
    pub value: u32,
}

impl urma_tp_feature_t {
    pub fn rm_multi_path(&self) -> bool {
        self.value & URMA_TP_FEAT_RM_MULTI_PATH != 0
    }

    pub fn rc_multi_path(&self) -> bool {
        self.value & URMA_TP_FEAT_RC_MULTI_PATH != 0
    }
}

/// C: `union urma_tp_type_en` (per-SL tp type enable, inside urma_sl_info_t)
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_tp_type_en_t {
    pub value: u32,
}

/// C: `struct urma_sl_info`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_sl_info_t {
    pub sl: u32, /* header field name: SL */
    pub tp_type: urma_tp_type_en_t,
}

/// C: `urma_port_attr_t` (enums modeled as c_int: C enums are 4 bytes here)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_port_attr_t {
    pub max_mtu: c_int,      /* urma_mtu_t */
    pub state: c_int,        /* urma_port_state_t */
    pub active_width: c_int, /* urma_link_width_t */
    pub active_speed: c_int, /* urma_speed_t */
    pub active_mtu: c_int,   /* urma_mtu_t */
}

/// C: `urma_guid_t`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_guid_t {
    pub raw: [u8; URMA_GUID_SIZE],
}

/// C: `urma_device_cap_t` — capabilities of one device. trans_mode is a bit
/// OR of urma_transport_mode_t; each mode has its own tp-type / order-type
/// cap; tp_feature carries per-mode multi-path support.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_device_cap_t {
    pub feature: urma_device_feature_t,
    pub max_jfc: u32,
    pub max_jfs: u32,
    pub max_jfr: u32,
    pub max_jetty: u32,
    pub max_jetty_grp: u32,
    pub max_jetty_in_jetty_grp: u32,
    pub max_jfc_depth: u32,
    pub max_jfs_depth: u32,
    pub max_jfr_depth: u32,
    pub max_jfs_inline_len: u32,
    pub max_jfs_sge: u32,
    pub max_jfs_rsge: u32,
    pub max_jfr_sge: u32,
    pub max_msg_size: u64,
    pub max_read_size: u32,
    pub max_write_size: u32,
    pub max_cas_size: u32,
    pub max_swap_size: u32,
    pub max_fetch_and_add_size: u32,
    pub max_fetch_and_sub_size: u32,
    pub max_fetch_and_and_size: u32,
    pub max_fetch_and_or_size: u32,
    pub max_fetch_and_xor_size: u32,
    pub atomic_feat: urma_atomic_feature_t,
    pub trans_mode: u16, /* bit OR of urma_transport_mode_t */
    pub reserved: u16,
    pub congestion_ctrl_alg: u16,
    pub ceq_cnt: u32,
    pub max_tp_in_tpg: u32,
    pub max_eid_cnt: u32,
    pub page_size_cap: u64,
    pub max_oor_cnt: u32,
    pub mn: u32,
    pub max_netaddr_cnt: u32,
    pub rm_order_cap: urma_order_type_cap_t,
    pub rc_order_cap: urma_order_type_cap_t,
    pub rm_tp_cap: urma_tp_type_cap_t,
    pub rc_tp_cap: urma_tp_type_cap_t,
    pub um_tp_cap: urma_tp_type_cap_t,
    pub tp_feature: urma_tp_feature_t,
    pub priority_info: [urma_sl_info_t; URMA_MAX_PRIORITY_CNT],
}

/// C: `urma_device_attr_t` — output of `urma_query_device` (caller allocates)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_device_attr_t {
    pub guid: urma_guid_t,
    pub dev_cap: urma_device_cap_t,
    pub port_cnt: u8,
    pub port_attr: [urma_port_attr_t; MAX_PORT_CNT],
    pub reserved_jetty_id_min: u32,
    pub reserved_jetty_id_max: u32,
}

/// Opaque: urma_context_t (contains pthread_mutex_t / fd, etc.), crosses the
/// API only as a pointer
#[repr(C)]
pub struct urma_context_t {
    _private: [u8; 0],
}

/// Opaque: urma_jfce_t (completion event fd), crosses the API only as a pointer
#[repr(C)]
pub struct urma_jfce_t {
    _private: [u8; 0],
}

/// C: `union urma_jfc_flag_t`
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_jfc_flag_t {
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_jfc_cfg_t {
    pub depth: u32,
    pub flag: urma_jfc_flag_t,
    pub ceqn: u32,
    pub jfce: *mut urma_jfce_t,
    pub user_ctx: u64,
}

/// Opaque: urma_jfc_t (contains pthread_mutex_t), crosses the API only as a pointer
#[repr(C)]
pub struct urma_jfc_t {
    _private: [u8; 0],
}

/// Identifier shared by jetty / jfs / jfr / jfc
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_jetty_id_t {
    pub eid: urma_eid_t,
    pub uasid: u32,
    pub id: u32,
}

/// C: `union urma_jfs_flag_t`.
/// Bit layout (urma_types.h): lock_free/error_suspend/outorder_comp 1 bit
/// each, order_type 8 bits (starting at bit3), multi_path at bit11.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_jfs_flag_t {
    pub value: u32,
}

impl urma_jfs_flag_t {
    pub fn order_type(&self) -> u32 {
        (self.value >> URMA_JFS_FLAG_ORDER_TYPE_SHIFT) & 0xff
    }

    pub fn with_order_type(mut self, v: u32) -> Self {
        self.value = (self.value & !(0xff << URMA_JFS_FLAG_ORDER_TYPE_SHIFT))
            | (v << URMA_JFS_FLAG_ORDER_TYPE_SHIFT);
        self
    }

    pub fn multi_path(&self) -> bool {
        self.value & URMA_JFS_FLAG_MULTI_PATH != 0
    }

    pub fn with_multi_path(mut self, on: bool) -> Self {
        if on {
            self.value |= URMA_JFS_FLAG_MULTI_PATH;
        } else {
            self.value &= !URMA_JFS_FLAG_MULTI_PATH;
        }
        self
    }
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_jfs_cfg_t {
    pub depth: u32,
    pub flag: urma_jfs_flag_t,
    pub trans_mode: u32, /* urma_transport_mode_t */
    pub priority: u8,
    pub max_sge: u8,
    pub max_rsge: u8,
    pub max_inline_data: u32,
    pub rnr_retry: u8,
    pub err_timeout: u8,
    pub jfc: *mut urma_jfc_t,
    pub user_ctx: u64,
}

/// C: `union urma_jfr_flag_t`. Default 0 = NO_TAG_MATCHING + order 0.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_jfr_flag_t {
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_jfr_cfg_t {
    pub id: u32, /* 0 means system-assigned */
    pub depth: u32,
    pub flag: urma_jfr_flag_t,
    pub trans_mode: u32, /* urma_transport_mode_t */
    pub max_sge: u8,
    pub min_rnr_timer: u8,
    pub jfc: *mut urma_jfc_t,
    pub token_value: urma_token_t,
    pub user_ctx: u64,
}

/// Opaque: urma_jfr_t (contains pthread_mutex_t), crosses the API only as a pointer
#[repr(C)]
pub struct urma_jfr_t {
    _private: [u8; 0],
}

/// C: `union urma_jetty_flag_t`
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_jetty_flag_t {
    pub value: u32,
}

/// The `shared` member (largest member, 16 bytes) of the anonymous union in
/// `urma_jetty_cfg_t`; the deprecated `jfr_cfg` pointer variant is not modeled.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_jetty_recv_cfg_t {
    pub jfr: *mut urma_jfr_t,
    pub jfc: *mut urma_jfc_t,
}

/// Opaque: urma_jetty_grp_t
#[repr(C)]
pub struct urma_jetty_grp_t {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_jetty_cfg_t {
    pub id: u32,
    pub flag: urma_jetty_flag_t,
    pub jfs_cfg: urma_jfs_cfg_t,
    pub recv: urma_jetty_recv_cfg_t,
    pub jetty_grp: *mut urma_jetty_grp_t,
    pub user_ctx: u64,
}

/// C: `urma_jetty_t`. Models only the documented prefix (urma_ctx / jetty_id /
/// remote_jetty); the remaining tail containing pthread_mutex_t is opaque.
/// Only accept pointers returned by `urma_create_jetty`; never construct this
/// type yourself.
#[repr(C)]
pub struct urma_jetty_t {
    pub urma_ctx: *mut urma_context_t,
    pub jetty_id: urma_jetty_id_t,
    pub remote_jetty: *mut urma_target_jetty_t,
    _opaque_tail: [u8; 0],
}

/// Opaque: urma_target_jetty_t (imported remote jetty), crosses the API only
/// as a pointer
#[repr(C)]
pub struct urma_target_jetty_t {
    _private: [u8; 0],
}

/// C: `union urma_import_jetty_flag_t`
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_import_jetty_flag_t {
    pub value: u32,
}

/// Request descriptor for importing a remote jetty
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_rjetty_t {
    pub jetty_id: urma_jetty_id_t,
    pub trans_mode: u32,          /* urma_transport_mode_t */
    pub policy: u32,              /* urma_jetty_grp_policy_t */
    pub type_: u32,               /* urma_target_type_t */
    pub flag: urma_import_jetty_flag_t,
    pub tp_type: u32,             /* urma_tp_type_t */
}

/// Security credential
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_token_t {
    pub token: u32,
}

/// Opaque: urma_token_id_t
#[repr(C)]
pub struct urma_token_id_t {
    _private: [u8; 0],
}

/// C: `union urma_reg_seg_flag_t`.
/// Bit layout: token_policy 3 bits (from bit0), cacheable bit3, dsva bit4,
/// access 6 bits (from bit5), non_pin bit11, user_iova bit12, token_id_valid
/// bit13.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_reg_seg_flag_t {
    pub value: u32,
}

impl urma_reg_seg_flag_t {
    const ACCESS_SHIFT: u32 = 5;
    const ACCESS_MASK: u32 = 0x3f << Self::ACCESS_SHIFT;

    /// The access field takes a combination of URMA_ACCESS_*
    pub fn access(&self) -> u32 {
        (self.value & Self::ACCESS_MASK) >> Self::ACCESS_SHIFT
    }

    pub fn with_access(mut self, access: u32) -> Self {
        self.value = (self.value & !Self::ACCESS_MASK) | (access << Self::ACCESS_SHIFT);
        self
    }
}

/// C: `union urma_seg_attr_t` (segment attributes inside urma_seg_t, same
/// layout as reg_seg_flag)
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_seg_attr_t {
    pub value: u32,
}

/// C: `union urma_import_seg_flag_t`.
/// Bit layout: cacheable bit0, access 6 bits (from bit1), mapping bit7.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_import_seg_flag_t {
    pub value: u32,
}

impl urma_import_seg_flag_t {
    const ACCESS_SHIFT: u32 = 1;
    const ACCESS_MASK: u32 = 0x3f << Self::ACCESS_SHIFT;

    /// The access field takes a combination of URMA_ACCESS_*
    pub fn access(&self) -> u32 {
        (self.value & Self::ACCESS_MASK) >> Self::ACCESS_SHIFT
    }

    pub fn with_access(mut self, access: u32) -> Self {
        self.value = (self.value & !Self::ACCESS_MASK) | (access << Self::ACCESS_SHIFT);
        self
    }
}

/// C: `urma_ubva_t`, `__attribute__((packed))` upstream: eid 16B + uasid 4B +
/// va 8B = 28 bytes, alignment 1. Reading/writing eid/uasid by value is safe;
/// va sits at an unaligned offset (+20) and must be accessed via [`Self::va`]
/// / [`Self::set_va`].
#[repr(C, packed)]
#[derive(Clone, Copy, Default)]
pub struct urma_ubva_t {
    pub eid: [u8; URMA_EID_SIZE],
    pub uasid: u32,
    pub va: u64,
}

impl urma_ubva_t {
    pub fn va(&self) -> u64 {
        unsafe { std::ptr::addr_of!(self.va).read_unaligned() }
    }

    pub fn set_va(&mut self, va: u64) {
        unsafe { std::ptr::addr_of_mut!(self.va).write_unaligned(va) }
    }
}

/// Public descriptor of registered memory (wire form), placed in the directory
/// for the peer's `urma_import_seg`
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_seg_t {
    pub ubva: urma_ubva_t,
    pub len: u64,
    pub attr: urma_seg_attr_t,
    pub token_id: u32,
}

/// Request parameters for registering memory
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_seg_cfg_t {
    pub va: u64,
    pub len: u64,
    pub token_id: *mut urma_token_id_t,
    pub token_value: urma_token_t,
    pub flag: urma_reg_seg_flag_t,
    pub user_ctx: u64,
    pub iova: u64,
}

/// Local memory segment handle obtained from register / import (all fields public)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_target_seg_t {
    pub seg: urma_seg_t,
    pub user_ctx: u64,
    pub mva: u64, /* mapped address when importing a remote segment */
    pub urma_ctx: *mut urma_context_t,
    pub token_id: *mut urma_token_id_t,
    pub handle: u64,
}

/// Opaque: urma_user_tseg_t (import-exemption path, unused)
#[repr(C)]
pub struct urma_user_tseg_t {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_sge_t {
    pub addr: u64,
    pub len: u32,
    pub tseg: *mut urma_target_seg_t,
    pub user_tseg: *mut urma_user_tseg_t,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_sg_t {
    pub sge: *mut urma_sge_t,
    pub num_sge: u32,
}

/// Payload of a READ/WRITE work request. READ semantics: src is the remote
/// address, dst is the local address.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_rw_wr_t {
    pub src: urma_sg_t,
    pub dst: urma_sg_t,
    pub target_hint: u8,
    pub notify_data: u64,
}

/// C: `union urma_jfs_wr_flag_t`
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_jfs_wr_flag_t {
    pub value: u32,
}

/// C: `urma_jfs_wr_t`. The payload union is modeled by its largest member
/// `urma_rw_wr_t`. READ/WRITE use `rw`; plain SEND can also be expressed —
/// `urma_send_wr_t.src` and `rw.src` both sit at union offset 0, and without
/// imm/invalidate the remaining members are all 0, so `rw.src` can be reused
/// (SEND with imm/invalidate and cas/faa are not modeled).
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_jfs_wr_t {
    pub opcode: u32, /* urma_opcode_t */
    pub flag: urma_jfs_wr_flag_t,
    pub tjetty: *mut urma_target_jetty_t,
    pub user_ctx: u64,
    pub rw: urma_rw_wr_t,
    pub next: *mut urma_jfs_wr_t,
}

/// C: `urma_jfr_wr_t` (JFR receive WR: hangs a locally registered buffer on
/// the JFR, where the peer's SEND data will land)
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_jfr_wr_t {
    pub src: urma_sg_t,
    pub user_ctx: u64,
    pub next: *mut urma_jfr_wr_t,
}

/// C: `union urma_cr_flag_t` (u8 bitfield)
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct urma_cr_flag_t {
    pub value: u8,
}

/// A completion record
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct urma_cr_t {
    pub status: c_int, /* urma_cr_status_t */
    pub user_ctx: u64,
    pub opcode: c_int, /* urma_cr_opcode_t, valid on the receive side only */
    pub flag: urma_cr_flag_t,
    pub completion_len: u32,
    pub local_id: u32,
    pub remote_id: urma_jetty_id_t,
    pub imm_data: u64,
    pub tpn: u32,
    pub user_data: usize,
}

/* ============================== Functions (urma_api.h) ============================== */

#[link(name = "urma")]
extern "C" {
    pub fn urma_init(conf: *mut urma_init_attr_t) -> urma_status_t;
    pub fn urma_uninit() -> urma_status_t;

    /* logging: the default sink is syslog; register a callback to see provider
     * messages elsewhere (the message arrives pre-formatted, no va_list) */
    pub fn urma_register_log_func(func: urma_log_cb_t) -> urma_status_t;
    pub fn urma_unregister_log_func() -> urma_status_t;
    pub fn urma_log_set_level(level: i32);
    pub fn urma_log_get_level() -> i32;

    pub fn urma_get_device_list(num_devices: *mut c_int) -> *mut *mut urma_device_t;
    pub fn urma_free_device_list(device_list: *mut *mut urma_device_t);
    pub fn urma_get_device_by_name(dev_name: *mut c_char) -> *mut urma_device_t;
    pub fn urma_get_eid_list(dev: *mut urma_device_t, cnt: *mut u32) -> *mut urma_eid_info_t;
    pub fn urma_free_eid_list(eid_list: *mut urma_eid_info_t);

    pub fn urma_query_device(
        dev: *mut urma_device_t,
        dev_attr: *mut urma_device_attr_t,
    ) -> urma_status_t;

    pub fn urma_create_context(dev: *mut urma_device_t, eid_index: u32) -> *mut urma_context_t;
    pub fn urma_delete_context(ctx: *mut urma_context_t) -> urma_status_t;

    pub fn urma_create_jfce(ctx: *mut urma_context_t) -> *mut urma_jfce_t;
    pub fn urma_delete_jfce(jfce: *mut urma_jfce_t) -> urma_status_t;

    pub fn urma_create_jfc(ctx: *mut urma_context_t, jfc_cfg: *mut urma_jfc_cfg_t) -> *mut urma_jfc_t;
    pub fn urma_delete_jfc(jfc: *mut urma_jfc_t) -> urma_status_t;
    pub fn urma_poll_jfc(jfc: *mut urma_jfc_t, cr_cnt: c_int, cr: *mut urma_cr_t) -> c_int;

    pub fn urma_create_jfr(ctx: *mut urma_context_t, jfr_cfg: *mut urma_jfr_cfg_t) -> *mut urma_jfr_t;
    pub fn urma_delete_jfr(jfr: *mut urma_jfr_t) -> urma_status_t;
    pub fn urma_post_jfr_wr(
        jfr: *mut urma_jfr_t,
        wr: *mut urma_jfr_wr_t,
        bad_wr: *mut *mut urma_jfr_wr_t,
    ) -> urma_status_t;

    pub fn urma_create_jetty(ctx: *mut urma_context_t, jetty_cfg: *mut urma_jetty_cfg_t) -> *mut urma_jetty_t;
    pub fn urma_delete_jetty(jetty: *mut urma_jetty_t) -> urma_status_t;

    pub fn urma_import_jetty(
        ctx: *mut urma_context_t,
        rjetty: *mut urma_rjetty_t,
        token_value: *mut urma_token_t,
    ) -> *mut urma_target_jetty_t;
    pub fn urma_unimport_jetty(tjetty: *mut urma_target_jetty_t) -> urma_status_t;

    pub fn urma_register_seg(ctx: *mut urma_context_t, seg_cfg: *mut urma_seg_cfg_t) -> *mut urma_target_seg_t;
    pub fn urma_unregister_seg(target_seg: *mut urma_target_seg_t) -> urma_status_t;
    pub fn urma_import_seg(
        ctx: *mut urma_context_t,
        seg: *mut urma_seg_t,
        token_value: *mut urma_token_t,
        addr: u64,
        flag: urma_import_seg_flag_t,
    ) -> *mut urma_target_seg_t;
    pub fn urma_unimport_seg(tseg: *mut urma_target_seg_t) -> urma_status_t;

    /* Export as opaque blobs: on bonding devices the blob appends the
       per-physical-device info (has_user_info ext), letting the importer
       resolve everything locally and skip the kernel-side seg/jetty
       exchange. Blobs are freed with the put fns; non-bonding devices
       export just the plain struct. */
    pub fn urma_get_seg_ctx(
        tseg: *mut urma_target_seg_t,
        seg: *mut *mut urma_seg_t,
        size: *mut u32,
    ) -> urma_status_t;
    pub fn urma_put_seg_ctx(seg: *mut urma_seg_t);
    pub fn urma_get_rjetty(
        jetty: *mut urma_jetty_t,
        rjetty: *mut *mut urma_rjetty_t,
        length: *mut u32,
    ) -> urma_status_t;
    pub fn urma_put_rjetty(rjetty: *mut urma_rjetty_t);

    pub fn urma_post_jetty_send_wr(
        jetty: *mut urma_jetty_t,
        wr: *mut urma_jfs_wr_t,
        bad_wr: *mut *mut urma_jfs_wr_t,
    ) -> urma_status_t;
}

/* ============================== Layout guards (tests) ============================== */

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    /// Expected values verified against the vendored C headers (LP64/AArch64).
    #[test]
    fn struct_sizes_and_alignments_match_c() {
        assert_eq!((size_of::<urma_eid_t>(), align_of::<urma_eid_t>()), (16, 8));
        assert_eq!((size_of::<urma_device_t>(), align_of::<urma_device_t>()), (4184, 8));
        assert_eq!((size_of::<urma_init_attr_t>(), align_of::<urma_init_attr_t>()), (16, 8));
        assert_eq!((size_of::<urma_eid_info_t>(), align_of::<urma_eid_info_t>()), (24, 8));
        assert_eq!((size_of::<urma_jfc_cfg_t>(), align_of::<urma_jfc_cfg_t>()), (32, 8));
        assert_eq!((size_of::<urma_jetty_id_t>(), align_of::<urma_jetty_id_t>()), (24, 8));
        assert_eq!((size_of::<urma_jfs_cfg_t>(), align_of::<urma_jfs_cfg_t>()), (40, 8));
        assert_eq!((size_of::<urma_jfr_cfg_t>(), align_of::<urma_jfr_cfg_t>()), (48, 8));
        assert_eq!((size_of::<urma_jetty_recv_cfg_t>(), align_of::<urma_jetty_recv_cfg_t>()), (16, 8));
        assert_eq!((size_of::<urma_jetty_cfg_t>(), align_of::<urma_jetty_cfg_t>()), (80, 8));
        assert_eq!((size_of::<urma_rjetty_t>(), align_of::<urma_rjetty_t>()), (48, 8));
        assert_eq!((size_of::<urma_seg_cfg_t>(), align_of::<urma_seg_cfg_t>()), (48, 8));
        assert_eq!((size_of::<urma_ubva_t>(), align_of::<urma_ubva_t>()), (28, 1));
        assert_eq!((size_of::<urma_seg_t>(), align_of::<urma_seg_t>()), (48, 8));
        assert_eq!((size_of::<urma_target_seg_t>(), align_of::<urma_target_seg_t>()), (88, 8));
        assert_eq!((size_of::<urma_sge_t>(), align_of::<urma_sge_t>()), (32, 8));
        assert_eq!((size_of::<urma_sg_t>(), align_of::<urma_sg_t>()), (16, 8));
        assert_eq!((size_of::<urma_rw_wr_t>(), align_of::<urma_rw_wr_t>()), (48, 8));
        assert_eq!((size_of::<urma_jfs_wr_t>(), align_of::<urma_jfs_wr_t>()), (80, 8));
        assert_eq!((size_of::<urma_jfr_wr_t>(), align_of::<urma_jfr_wr_t>()), (32, 8));
        assert_eq!((size_of::<urma_cr_t>(), align_of::<urma_cr_t>()), (80, 8));

        // device-capability structs (urma_query_device output)
        assert_eq!((size_of::<urma_sl_info_t>(), align_of::<urma_sl_info_t>()), (8, 4));
        assert_eq!((size_of::<urma_port_attr_t>(), align_of::<urma_port_attr_t>()), (20, 4));
        assert_eq!((size_of::<urma_guid_t>(), align_of::<urma_guid_t>()), (16, 1));
        assert_eq!((size_of::<urma_device_cap_t>(), align_of::<urma_device_cap_t>()), (304, 8));
        assert_eq!((size_of::<urma_device_attr_t>(), align_of::<urma_device_attr_t>()), (496, 8));
    }

    /// Critical field offsets the hand-written model depends on (including
    /// where unions are expanded by their largest member)
    #[test]
    fn critical_field_offsets_match_c() {
        // urma_jetty_t prefix: jetty_id follows the urma_ctx pointer
        assert_eq!(offset_of!(urma_jetty_t, jetty_id), 8);
        assert_eq!(offset_of!(urma_jetty_t, remote_jetty), 32);

        // urma_jetty_cfg_t: the anonymous union (recv) after jfs_cfg spans 48..64
        assert_eq!(offset_of!(urma_jetty_cfg_t, jfs_cfg), 8);
        assert_eq!(offset_of!(urma_jetty_cfg_t, recv), 48);
        assert_eq!(offset_of!(urma_jetty_cfg_t, jetty_grp), 64);
        assert_eq!(offset_of!(urma_jetty_cfg_t, user_ctx), 72);

        // urma_jfs_wr_t: the payload union (rw) spans 24..72, next follows
        assert_eq!(offset_of!(urma_jfs_wr_t, tjetty), 8);
        assert_eq!(offset_of!(urma_jfs_wr_t, rw), 24);
        assert_eq!(offset_of!(urma_jfs_wr_t, next), 72);

        // urma_jfr_wr_t: user_ctx and next follow src (16B) in order
        assert_eq!(offset_of!(urma_jfr_wr_t, user_ctx), 16);
        assert_eq!(offset_of!(urma_jfr_wr_t, next), 24);

        // urma_seg_t: len aligns to 32 after the packed ubva (28B)
        assert_eq!(offset_of!(urma_seg_t, len), 32);
        assert_eq!(offset_of!(urma_seg_t, attr), 40);
        assert_eq!(offset_of!(urma_seg_t, token_id), 44);

        // Other structs with offsets we depend on
        assert_eq!(offset_of!(urma_target_seg_t, user_ctx), 48);
        assert_eq!(offset_of!(urma_target_seg_t, mva), 56);
        assert_eq!(offset_of!(urma_cr_t, user_ctx), 8);
        assert_eq!(offset_of!(urma_cr_t, completion_len), 24);
        assert_eq!(offset_of!(urma_cr_t, remote_id), 32);
        assert_eq!(offset_of!(urma_cr_t, tpn), 64);
        assert_eq!(offset_of!(urma_jfr_cfg_t, jfc), 24);
        assert_eq!(offset_of!(urma_jfr_cfg_t, token_value), 32);
        assert_eq!(offset_of!(urma_jfs_cfg_t, jfc), 24);
        assert_eq!(offset_of!(urma_jfc_cfg_t, jfce), 16);

        // urma_device_cap_t: u64 fields force the padding at 52..56 and
        // 124..128; the per-mode caps sit behind the scalar tail
        assert_eq!(offset_of!(urma_device_cap_t, max_msg_size), 56);
        assert_eq!(offset_of!(urma_device_cap_t, trans_mode), 104);
        assert_eq!(offset_of!(urma_device_cap_t, page_size_cap), 128);
        assert_eq!(offset_of!(urma_device_cap_t, rm_order_cap), 148);
        assert_eq!(offset_of!(urma_device_cap_t, rm_tp_cap), 156);
        assert_eq!(offset_of!(urma_device_cap_t, rc_tp_cap), 160);
        assert_eq!(offset_of!(urma_device_cap_t, um_tp_cap), 164);
        assert_eq!(offset_of!(urma_device_cap_t, tp_feature), 168);
        assert_eq!(offset_of!(urma_device_cap_t, priority_info), 172);

        // urma_device_attr_t: dev_cap follows the 16-byte guid; port_cnt
        // (u8) leaves 3 padding bytes before the 4-aligned port_attr array
        assert_eq!(offset_of!(urma_device_attr_t, dev_cap), 16);
        assert_eq!(offset_of!(urma_device_attr_t, port_cnt), 320);
        assert_eq!(offset_of!(urma_device_attr_t, port_attr), 324);
        assert_eq!(offset_of!(urma_device_attr_t, reserved_jetty_id_min), 484);
    }

    #[test]
    fn cap_union_helpers() {
        let f = urma_device_feature_t::default().with_ctp_en(true);
        assert!(f.ctp_en());
        assert_eq!(f.value, URMA_FEATURE_CTP_EN);
        assert!(!urma_device_feature_t::default().ctp_en());

        let c = urma_tp_type_cap_t { value: URMA_TP_TYPE_CAP_RTP | URMA_TP_TYPE_CAP_CTP };
        assert!(c.rtp());
        assert!(c.ctp());
        assert!(!c.utp());

        let o = urma_order_type_cap_t { value: URMA_ORDER_CAP_OT | URMA_ORDER_CAP_NO };
        assert!(o.ot());
        assert!(o.no());
        assert!(!o.oi());
        assert!(!o.ol());

        let t = urma_tp_feature_t { value: URMA_TP_FEAT_RM_MULTI_PATH };
        assert!(t.rm_multi_path());
        assert!(!t.rc_multi_path());
    }

    #[test]
    fn flag_helpers_round_trip() {
        let f = urma_reg_seg_flag_t::default().with_access(URMA_ACCESS_READ | URMA_ACCESS_WRITE);
        assert_eq!(f.access(), URMA_ACCESS_READ | URMA_ACCESS_WRITE);
        assert_eq!(f.value, (URMA_ACCESS_READ | URMA_ACCESS_WRITE) << 5);

        let f = urma_import_seg_flag_t::default().with_access(URMA_ACCESS_READ | URMA_ACCESS_WRITE);
        assert_eq!(f.access(), URMA_ACCESS_READ | URMA_ACCESS_WRITE);
        assert_eq!(f.value, (URMA_ACCESS_READ | URMA_ACCESS_WRITE) << 1);

        let f = urma_jfs_flag_t::default().with_order_type(2).with_multi_path(true);
        assert_eq!(f.order_type(), 2);
        assert!(f.multi_path());
        assert_eq!(f.value, (2 << 3) | URMA_JFS_FLAG_MULTI_PATH);

        assert!(!urma_jfs_flag_t::default().with_multi_path(false).multi_path());
    }

    #[test]
    fn packed_ubva_accessors() {
        let mut ubva = urma_ubva_t { eid: [0xa5; URMA_EID_SIZE], uasid: 0x1234, ..Default::default() };
        ubva.set_va(0xdead_beef_cafe_f00d);
        assert_eq!(ubva.eid, [0xa5; URMA_EID_SIZE]);
        let uasid = ubva.uasid; /* copy packed field before asserting (assert_eq! takes a reference) */
        assert_eq!(uasid, 0x1234);
        assert_eq!(ubva.va(), 0xdead_beef_cafe_f00d);
    }
}
