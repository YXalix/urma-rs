//! Error types.

use std::fmt;

use crate::ffi;

#[derive(Debug)]
pub enum Error {
    /// Device missing etc., with a human-readable message
    NotFound(String),
    /// A create-style API returned NULL; errno is captured at the failure site
    /// (0 when the C library does not set one)
    Null(&'static str, i32),
    /// A urma call returned non-URMA_SUCCESS
    Status(i32, &'static str),
    /// Timed out polling for a completion
    PollTimeout { user_ctx: u64 },
    /// Completion check failed (status not SUCCESS or user_ctx mismatch)
    BadCompletion { status: i32, user_ctx: u64 },
    /// Invalid argument
    Invalid(String),
    /// IO (socket etc.)
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound(m) => write!(f, "not found: {m}"),
            Error::Null(what, errno) if *errno == 0 => write!(f, "{what} returned NULL"),
            Error::Null(what, errno) => write!(
                f,
                "{what} returned NULL (errno {errno}: {})",
                std::io::Error::from_raw_os_error(*errno)
            ),
            Error::Status(st, what) => write!(f, "{what} failed, status {st}"),
            Error::PollTimeout { user_ctx } => {
                write!(f, "poll completion timeout (user_ctx 0x{user_ctx:x})")
            }
            Error::BadCompletion { status, user_ctx } => {
                write!(f, "bad completion (status {status}, user_ctx 0x{user_ctx:x})")
            }
            Error::Invalid(m) => write!(f, "invalid: {m}"),
            Error::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn check_status(st: i32, what: &'static str) -> Result<()> {
    if st == ffi::URMA_SUCCESS {
        Ok(())
    } else {
        Err(Error::Status(st, what))
    }
}
