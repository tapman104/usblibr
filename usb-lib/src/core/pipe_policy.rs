/// Identifies which pipe policy to read (used with `get_pipe_policy`).
///
/// Each variant corresponds to one of the WinUSB `PIPE_TRANSFER_*` policy
/// constants; other platforms may support a subset of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipePolicyKind {
    /// Terminate an OUT transfer with a zero-length packet when the transfer
    /// length is a multiple of the max packet size (OUT pipes only).
    ShortPacketTerminate,
    /// Automatically issue a clear-stall request when an IN pipe stalls,
    /// without requiring a `reset_pipe` call (IN pipes only).
    AutoClearStall,
    /// Per-pipe I/O timeout in milliseconds. 0 disables the timeout.
    TransferTimeout,
    /// Allow short IN packets; if false the pipe returns an error on short reads.
    AllowPartialReads,
    /// When `AllowPartialReads` is true, automatically flush unread partial
    /// data at the start of the next transfer (IN pipes only).
    AutoFlush,
    /// Enable raw I/O mode — transfers go directly to the kernel, bypassing
    /// buffering (IN pipes only; requires `AllowPartialReads = false`).
    RawIo,
    /// Reset the pipe state when the device resumes from suspend.
    ResetPipeOnResume,
}

/// A pipe policy value — returned by `get_pipe_policy` and accepted by
/// `set_pipe_policy`.  Each variant carries its own payload so callers never
/// need to handle raw bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipePolicy {
    ShortPacketTerminate(bool),
    AutoClearStall(bool),
    /// Transfer timeout in milliseconds (`0` = no timeout).
    TransferTimeout(u32),
    AllowPartialReads(bool),
    AutoFlush(bool),
    RawIo(bool),
    ResetPipeOnResume(bool),
}

impl PipePolicy {
    /// Return the `PipePolicyKind` discriminant for this policy value.
    pub fn kind(&self) -> PipePolicyKind {
        match self {
            Self::ShortPacketTerminate(_) => PipePolicyKind::ShortPacketTerminate,
            Self::AutoClearStall(_) => PipePolicyKind::AutoClearStall,
            Self::TransferTimeout(_) => PipePolicyKind::TransferTimeout,
            Self::AllowPartialReads(_) => PipePolicyKind::AllowPartialReads,
            Self::AutoFlush(_) => PipePolicyKind::AutoFlush,
            Self::RawIo(_) => PipePolicyKind::RawIo,
            Self::ResetPipeOnResume(_) => PipePolicyKind::ResetPipeOnResume,
        }
    }

    /// Extract the boolean payload for bool-valued policies.
    /// Returns `None` for `TransferTimeout` (which is u32-valued).
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::ShortPacketTerminate(v)
            | Self::AutoClearStall(v)
            | Self::AllowPartialReads(v)
            | Self::AutoFlush(v)
            | Self::RawIo(v)
            | Self::ResetPipeOnResume(v) => Some(*v),
            Self::TransferTimeout(_) => None,
        }
    }
}
