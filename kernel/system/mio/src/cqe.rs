//! Completion Queue Entry (CQE) — result of a completed I/O operation.
//!
//! Each CQE is 16 bytes (compact, cache-friendly). It carries:
//! - `user_data`: the opaque token from the matching SQE.
//! - `result`: bytes transferred on success, negative errno on failure.
//! - `flags`: per-CQE flags (e.g., buffer ID for BUFFER_SELECT).

/// Per-CQE flags.
pub mod cqe_flags {
    /// The CQE carries a buffer ID in the upper 16 bits of `flags`.
    pub const BUFFER: u32 = 1 << 0;
    /// More CQEs are coming for this SQE (multi-shot operations).
    pub const MORE: u32   = 1 << 1;
    /// The socket has become ready for notification (used with multishot accept).
    pub const SOCK_NONEMPTY: u32 = 1 << 2;
    /// This CQE is a notification, not a completion (used internally).
    pub const NOTIF: u32  = 1 << 3;
}

/// A Completion Queue Entry.
///
/// 16 bytes — returned by the kernel after an I/O operation completes.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct CompletionEntry {
    /// Opaque user data copied from the matching SQE.
    pub user_data: u64,
    /// Result: bytes transferred (≥ 0) or negative errno (< 0).
    pub result: i32,
    /// Per-CQE flags.
    pub flags: u32,
}

impl CompletionEntry {
    /// Create a zeroed CQE.
    pub const fn zeroed() -> Self {
        CompletionEntry {
            user_data: 0,
            result: 0,
            flags: 0,
        }
    }

    /// Create a success CQE.
    #[inline]
    pub fn success(user_data: u64, bytes: i32) -> Self {
        CompletionEntry {
            user_data,
            result: bytes,
            flags: 0,
        }
    }

    /// Create an error CQE.
    #[inline]
    pub fn error(user_data: u64, errno: i32) -> Self {
        CompletionEntry {
            user_data,
            result: errno, // negative
            flags: 0,
        }
    }

    /// Check if this CQE indicates success.
    #[inline]
    pub fn is_success(&self) -> bool {
        self.result >= 0
    }

    /// Check if this CQE indicates an error.
    #[inline]
    pub fn is_error(&self) -> bool {
        self.result < 0
    }

    /// Extract the buffer ID from flags (valid only if BUFFER flag is set).
    #[inline]
    pub fn buffer_id(&self) -> Option<u16> {
        if self.flags & cqe_flags::BUFFER != 0 {
            Some((self.flags >> 16) as u16)
        } else {
            None
        }
    }

    /// Check if more CQEs are coming for the same SQE (multi-shot).
    #[inline]
    pub fn has_more(&self) -> bool {
        self.flags & cqe_flags::MORE != 0
    }
}
