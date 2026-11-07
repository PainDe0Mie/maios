//! Submission Queue Entry (SQE) — describes a single I/O operation.
//!
//! Layout is designed for cache-line efficiency (64 bytes per SQE, matching
//! io_uring's struct io_uring_sqe).
//!
//! Each SQE encodes:
//! - **opcode**: which I/O operation to perform.
//! - **flags**: per-SQE flags (linked, drain, etc.).
//! - **fd**: file descriptor for the target.
//! - **addr/len**: buffer address and length (or buffer pool index for registered I/O).
//! - **offset**: file offset for positional I/O.
//! - **user_data**: opaque token returned in the corresponding CQE.

/// I/O operation opcodes.
///
/// Modelled after io_uring opcodes with MaiOS-specific extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    /// No-op (useful for testing and linked-op chains).
    Nop = 0,
    /// Vectored read from fd at offset.
    Read = 1,
    /// Vectored write to fd at offset.
    Write = 2,
    /// fsync / fdatasync.
    Fsync = 3,
    /// Poll for events on fd.
    PollAdd = 4,
    /// Remove a pending poll request.
    PollRemove = 5,
    /// Accept a connection on a listening socket.
    Accept = 6,
    /// Connect a socket to a remote address.
    Connect = 7,
    /// Close a file descriptor.
    Close = 8,
    /// Send data on a socket.
    Send = 9,
    /// Receive data from a socket.
    Recv = 10,
    /// Open a file (path-based).
    OpenAt = 11,
    /// Get file status (fstat).
    Statx = 12,
    /// Read from a registered buffer (zero-copy path).
    ReadFixed = 13,
    /// Write from a registered buffer (zero-copy path).
    WriteFixed = 14,
    /// Cancel a pending operation by user_data.
    Cancel = 15,
    /// Timeout: complete after a duration or N completions.
    Timeout = 16,
    /// Remove a pending timeout.
    TimeoutRemove = 17,
    /// Link a timeout to the previous SQE.
    LinkTimeout = 18,
    /// Splice data between two fds (zero-copy pipe).
    Splice = 19,
    /// Provide buffers to the kernel buffer pool.
    ProvideBuffers = 20,
    /// Remove buffers from the kernel buffer pool.
    RemoveBuffers = 21,
    /// MaiOS-specific: submit a compute task to MHC.
    MhcSubmit = 128,
    /// MaiOS-specific: yield the current timeslice to MKS.
    MksYield = 129,
    /// MaiOS-specific: flush NVMe submission queue.
    NvmeFlush = 130,
}

/// Per-SQE flags.
pub mod sqe_flags {
    /// This SQE is linked to the next: if this fails, cancel the chain.
    pub const IO_LINK: u8    = 1 << 0;
    /// Hard link: always execute the next SQE regardless of this one's result.
    pub const IO_HARDLINK: u8 = 1 << 1;
    /// Drain: wait for all prior SQEs to complete before starting this one.
    pub const IO_DRAIN: u8   = 1 << 2;
    /// Use a registered buffer (addr is buffer index, not pointer).
    pub const FIXED_FILE: u8 = 1 << 3;
    /// Async: force this SQE to be processed by a worker thread.
    pub const ASYNC: u8      = 1 << 4;
    /// Buffer select: let the kernel pick a buffer from the provided pool.
    pub const BUFFER_SELECT: u8 = 1 << 5;
}

/// Fsync flags (used with OpCode::Fsync).
pub mod fsync_flags {
    /// Only sync data, not metadata (fdatasync semantics).
    pub const DATASYNC: u32 = 1 << 0;
}

/// A Submission Queue Entry.
///
/// 64 bytes, cache-line aligned for optimal ring buffer performance.
#[derive(Clone)]
#[repr(C)]
pub struct SubmissionEntry {
    /// Operation code.
    pub opcode: OpCode,
    /// Per-SQE flags.
    pub flags: u8,
    /// I/O priority (ioprio_class << 13 | ioprio_data).
    pub ioprio: u16,
    /// Target file descriptor (or fixed-file index if FIXED_FILE is set).
    pub fd: i32,
    /// File offset for positional I/O (or timeout spec for Timeout ops).
    pub offset: u64,
    /// Buffer address (userspace pointer or registered buffer index).
    pub addr: u64,
    /// Buffer length in bytes.
    pub len: u32,
    /// Operation-specific flags (e.g., fsync_flags, poll_events).
    pub op_flags: u32,
    /// Opaque user data — returned verbatim in the matching CQE.
    pub user_data: u64,
    /// Buffer pool group ID (for BUFFER_SELECT).
    pub buf_group: u16,
    /// Personality / credentials index.
    pub personality: u16,
    /// Splice: destination fd (for Splice opcode).
    pub splice_fd_in: i32,
    /// Reserved for future use / padding.
    pub _pad: [u64; 1],
}

impl SubmissionEntry {
    /// Create a zeroed SQE.
    pub const fn zeroed() -> Self {
        SubmissionEntry {
            opcode: OpCode::Nop,
            flags: 0,
            ioprio: 0,
            fd: -1,
            offset: 0,
            addr: 0,
            len: 0,
            op_flags: 0,
            user_data: 0,
            buf_group: 0,
            personality: 0,
            splice_fd_in: -1,
            _pad: [0; 1],
        }
    }

    // -----------------------------------------------------------------------
    // Builder methods for common operations
    // -----------------------------------------------------------------------

    /// Build a read SQE.
    pub fn read(fd: i32, addr: u64, len: u32, offset: u64, user_data: u64) -> Self {
        SubmissionEntry {
            opcode: OpCode::Read,
            fd,
            addr,
            len,
            offset,
            user_data,
            ..Self::zeroed()
        }
    }

    /// Build a write SQE.
    pub fn write(fd: i32, addr: u64, len: u32, offset: u64, user_data: u64) -> Self {
        SubmissionEntry {
            opcode: OpCode::Write,
            fd,
            addr,
            len,
            offset,
            user_data,
            ..Self::zeroed()
        }
    }

    /// Build an fsync SQE.
    pub fn fsync(fd: i32, datasync: bool, user_data: u64) -> Self {
        SubmissionEntry {
            opcode: OpCode::Fsync,
            fd,
            op_flags: if datasync { fsync_flags::DATASYNC } else { 0 },
            user_data,
            ..Self::zeroed()
        }
    }

    /// Build a close SQE.
    pub fn close(fd: i32, user_data: u64) -> Self {
        SubmissionEntry {
            opcode: OpCode::Close,
            fd,
            user_data,
            ..Self::zeroed()
        }
    }

    /// Build a nop SQE (useful for testing or chain padding).
    pub fn nop(user_data: u64) -> Self {
        SubmissionEntry {
            opcode: OpCode::Nop,
            user_data,
            ..Self::zeroed()
        }
    }

    /// Build a zero-copy read from a registered buffer.
    pub fn read_fixed(fd: i32, buf_index: u16, len: u32, offset: u64, user_data: u64) -> Self {
        SubmissionEntry {
            opcode: OpCode::ReadFixed,
            fd,
            addr: buf_index as u64,
            len,
            offset,
            user_data,
            flags: sqe_flags::FIXED_FILE,
            ..Self::zeroed()
        }
    }

    /// Build a zero-copy write from a registered buffer.
    pub fn write_fixed(fd: i32, buf_index: u16, len: u32, offset: u64, user_data: u64) -> Self {
        SubmissionEntry {
            opcode: OpCode::WriteFixed,
            fd,
            addr: buf_index as u64,
            len,
            offset,
            user_data,
            flags: sqe_flags::FIXED_FILE,
            ..Self::zeroed()
        }
    }

    /// Build a cancel SQE (cancel a pending op by its user_data).
    pub fn cancel(target_user_data: u64, user_data: u64) -> Self {
        SubmissionEntry {
            opcode: OpCode::Cancel,
            addr: target_user_data,
            user_data,
            ..Self::zeroed()
        }
    }

    /// Set the IO_LINK flag (chain to next SQE).
    #[inline]
    pub fn linked(mut self) -> Self {
        self.flags |= sqe_flags::IO_LINK;
        self
    }

    /// Set the IO_DRAIN flag (barrier: wait for all prior SQEs).
    #[inline]
    pub fn drain(mut self) -> Self {
        self.flags |= sqe_flags::IO_DRAIN;
        self
    }

    /// Set the ASYNC flag (force worker thread dispatch).
    #[inline]
    pub fn async_dispatch(mut self) -> Self {
        self.flags |= sqe_flags::ASYNC;
        self
    }
}
