//! Error types shared by the patch engine.

use std::fmt;

/// All failures surfaced by the engine are classified by [`ErrorKind`] so the
/// CLI can map them to stable process exit codes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    pub message: String,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error { kind, message: message.into() }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.label(), self.message)
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// Root directory does not exist or is not a directory.
    InvalidRoot,
    /// Request escapes the project root (traversal, absolute path, symlink).
    UnsafePath,
    /// Plan itself is malformed (bad JSON, unknown op, same-file conflicts...).
    InvalidPlan,
    /// A text snippet is absent or appears multiple times; overlaps rejected.
    Ambiguous,
    /// On-disk content no longer matches what the plan was based on.
    Conflict,
    /// Another writer session holds the directory.
    Locked,
    /// A journal in the interrupted/conflicting state blocks the operation.
    PendingRecovery,
    /// Crash hook requested an abort (used for injectable interruptions).
    Interrupted,
    /// Anything that is an unexpected OS-level failure.
    Io,
}

impl ErrorKind {
    pub fn label(self) -> &'static str {
        match self {
            ErrorKind::InvalidRoot => "invalid root",
            ErrorKind::UnsafePath => "unsafe path",
            ErrorKind::InvalidPlan => "invalid plan",
            ErrorKind::Ambiguous => "ambiguous patch",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Locked => "locked",
            ErrorKind::PendingRecovery => "pending recovery",
            ErrorKind::Interrupted => "interrupted",
            ErrorKind::Io => "io error",
        }
    }

    /// Stable process exit code used by the `atp` binary.
    pub fn exit_code(self) -> i32 {
        match self {
            ErrorKind::InvalidRoot => 2,
            ErrorKind::UnsafePath => 3,
            ErrorKind::InvalidPlan => 4,
            ErrorKind::Ambiguous => 5,
            ErrorKind::Conflict => 6,
            ErrorKind::Locked => 7,
            ErrorKind::PendingRecovery => 8,
            ErrorKind::Interrupted => 9,
            ErrorKind::Io => 10,
        }
    }
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn io_err(err: std::io::Error, context: impl Into<String>) -> Error {
    let mut msg = context.into();
    msg.push_str(": ");
    msg.push_str(&err.to_string());
    Error::new(ErrorKind::Io, msg)
}
