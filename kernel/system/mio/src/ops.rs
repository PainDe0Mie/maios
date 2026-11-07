//! I/O operation dispatch and execution.
//!
//! Each opcode in the SQE maps to a handler function here. The dispatcher
//! routes SQEs to the appropriate handler and returns the result code
//! that will be placed in the CQE.
//!
//! Operation handlers are intentionally minimal stubs in Phase 1 —
//! they will be wired to actual MaiOS subsystems (VFS, NVMe, network stack)
//! as those become available.
//!
//! Design principle: each handler is a pure function
//! `fn(&SubmissionEntry) -> i32` so it can be called from any context
//! (inline, worker thread, SQPOLL thread) without additional state.

use crate::sqe::{SubmissionEntry, OpCode};
use crate::MioError;

/// Dispatch an SQE to the appropriate operation handler.
///
/// Returns the result code for the CQE:
/// - `>= 0`: bytes transferred or success indicator.
/// - `< 0`: negative errno on failure.
pub fn dispatch(sqe: &SubmissionEntry) -> i32 {
    match sqe.opcode {
        OpCode::Nop           => op_nop(sqe),
        OpCode::Read          => op_read(sqe),
        OpCode::Write         => op_write(sqe),
        OpCode::Fsync         => op_fsync(sqe),
        OpCode::PollAdd       => op_poll_add(sqe),
        OpCode::PollRemove    => op_poll_remove(sqe),
        OpCode::Accept        => op_accept(sqe),
        OpCode::Connect       => op_connect(sqe),
        OpCode::Close         => op_close(sqe),
        OpCode::Send          => op_send(sqe),
        OpCode::Recv          => op_recv(sqe),
        OpCode::OpenAt        => op_open_at(sqe),
        OpCode::Statx         => op_statx(sqe),
        OpCode::ReadFixed     => op_read_fixed(sqe),
        OpCode::WriteFixed    => op_write_fixed(sqe),
        OpCode::Cancel        => op_cancel(sqe),
        OpCode::Timeout       => op_timeout(sqe),
        OpCode::TimeoutRemove => op_timeout_remove(sqe),
        OpCode::LinkTimeout   => op_link_timeout(sqe),
        OpCode::Splice        => op_splice(sqe),
        OpCode::ProvideBuffers => op_provide_buffers(sqe),
        OpCode::RemoveBuffers  => op_remove_buffers(sqe),
        OpCode::MhcSubmit     => op_mhc_submit(sqe),
        OpCode::MksYield      => op_mks_yield(sqe),
        OpCode::NvmeFlush     => op_nvme_flush(sqe),
    }
}

// ---------------------------------------------------------------------------
// Operation handlers
// ---------------------------------------------------------------------------

/// NOP — always succeeds, returns 0.
fn op_nop(_sqe: &SubmissionEntry) -> i32 {
    0
}

/// Read from fd at offset into user buffer.
///
/// Phase 1: validates parameters and returns the requested length
/// (simulating a successful read). Will be wired to VFS in Phase 2.
fn op_read(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    if sqe.addr == 0 || sqe.len == 0 {
        return MioError::InvalidArg.as_i32();
    }
    // Phase 1 stub: simulate successful read of `len` bytes.
    sqe.len as i32
}

/// Write from user buffer to fd at offset.
fn op_write(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    if sqe.addr == 0 || sqe.len == 0 {
        return MioError::InvalidArg.as_i32();
    }
    sqe.len as i32
}

/// Fsync/fdatasync on fd.
fn op_fsync(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    0
}

/// Add a poll monitor on fd.
fn op_poll_add(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    // op_flags contains the poll event mask.
    0
}

/// Remove a pending poll.
fn op_poll_remove(_sqe: &SubmissionEntry) -> i32 {
    0
}

/// Accept a connection.
fn op_accept(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    // Return a new fd (stub: fd 3).
    3
}

/// Connect a socket.
fn op_connect(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    0
}

/// Close a file descriptor.
fn op_close(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    0
}

/// Send data on a socket.
fn op_send(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    sqe.len as i32
}

/// Receive data from a socket.
fn op_recv(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    sqe.len as i32
}

/// Open a file by path.
fn op_open_at(sqe: &SubmissionEntry) -> i32 {
    if sqe.addr == 0 {
        return MioError::InvalidArg.as_i32();
    }
    // Return a new fd (stub: fd 4).
    4
}

/// Get file status.
fn op_statx(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 && sqe.addr == 0 {
        return MioError::InvalidArg.as_i32();
    }
    0
}

/// Read using a registered (fixed) buffer — zero-copy path.
fn op_read_fixed(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    sqe.len as i32
}

/// Write using a registered (fixed) buffer — zero-copy path.
fn op_write_fixed(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    sqe.len as i32
}

/// Cancel a pending operation identified by user_data.
fn op_cancel(_sqe: &SubmissionEntry) -> i32 {
    // addr field contains the target user_data.
    0
}

/// Timeout operation.
fn op_timeout(_sqe: &SubmissionEntry) -> i32 {
    // offset contains the timeout in nanoseconds.
    MioError::TimedOut.as_i32()
}

/// Remove a pending timeout.
fn op_timeout_remove(_sqe: &SubmissionEntry) -> i32 {
    0
}

/// Link a timeout to the previous SQE.
fn op_link_timeout(_sqe: &SubmissionEntry) -> i32 {
    0
}

/// Splice data between two fds (zero-copy).
fn op_splice(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 || sqe.splice_fd_in < 0 {
        return MioError::BadFd.as_i32();
    }
    sqe.len as i32
}

/// Provide buffers to a kernel buffer pool.
fn op_provide_buffers(sqe: &SubmissionEntry) -> i32 {
    if sqe.addr == 0 || sqe.len == 0 {
        return MioError::InvalidArg.as_i32();
    }
    0
}

/// Remove buffers from a kernel buffer pool.
fn op_remove_buffers(_sqe: &SubmissionEntry) -> i32 {
    0
}

/// MaiOS-specific: submit a compute task to MHC (GPU subsystem).
fn op_mhc_submit(sqe: &SubmissionEntry) -> i32 {
    if sqe.addr == 0 {
        return MioError::InvalidArg.as_i32();
    }
    // Phase 2: will call into mhc::submit_task().
    0
}

/// MaiOS-specific: yield the current timeslice.
fn op_mks_yield(_sqe: &SubmissionEntry) -> i32 {
    // Phase 2: will call into mks::get().tick() or task::yield_now().
    0
}

/// MaiOS-specific: flush NVMe submission queue.
fn op_nvme_flush(sqe: &SubmissionEntry) -> i32 {
    if sqe.fd < 0 {
        return MioError::BadFd.as_i32();
    }
    0
}
