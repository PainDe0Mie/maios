//! Unified error handling for MaiOS syscalls.
//!
//! Defines a canonical `SyscallError` enum with conversions to both
//! Linux errno values (negative i64) and Windows NTSTATUS codes.

/// Unified error codes for all MaiOS kernel syscall operations.
///
/// Each variant maps to both a Linux errno and a Windows NT STATUS code,
/// allowing a single syscall implementation to serve both ABIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyscallError {
    /// Syscall not implemented (ENOSYS / STATUS_NOT_IMPLEMENTED)
    NotImplemented,
    /// Invalid argument (EINVAL / STATUS_INVALID_PARAMETER)
    InvalidArgument,
    /// Bad file descriptor or handle (EBADF / STATUS_INVALID_HANDLE)
    BadFileDescriptor,
    /// Permission denied (EACCES / STATUS_ACCESS_DENIED)
    PermissionDenied,
    /// File or path not found (ENOENT / STATUS_OBJECT_NAME_NOT_FOUND)
    NotFound,
    /// Out of memory (ENOMEM / STATUS_NO_MEMORY)
    OutOfMemory,
    /// I/O error (EIO / STATUS_IO_DEVICE_ERROR)
    IoError,
    /// Not a directory (ENOTDIR / STATUS_NOT_A_DIRECTORY)
    NotADirectory,
    /// Is a directory (EISDIR / STATUS_FILE_IS_A_DIRECTORY)
    IsADirectory,
    /// File already exists (EEXIST / STATUS_OBJECT_NAME_COLLISION)
    FileExists,
    /// Not an executable format (ENOEXEC / STATUS_INVALID_IMAGE_FORMAT)
    NotExecutable,
    /// No such device (ENODEV / STATUS_NO_SUCH_DEVICE)
    NoDevice,
    /// Inappropriate ioctl for device (ENOTTY / STATUS_INVALID_DEVICE_REQUEST)
    NotATerminal,
    /// Resource busy (EBUSY / STATUS_DEVICE_BUSY)
    Busy,
    /// Operation would block (EAGAIN / STATUS_PENDING)
    WouldBlock,
    /// Bad address / access violation (EFAULT / STATUS_ACCESS_VIOLATION)
    Fault,
    /// No space left on device (ENOSPC / STATUS_DISK_FULL)
    NoSpace,
    /// Illegal seek (ESPIPE / STATUS_PIPE_BROKEN)
    IllegalSeek,
    /// Interrupted syscall (EINTR / STATUS_CANCELLED)
    Interrupted,
    /// Read-only filesystem (EROFS / STATUS_MEDIA_WRITE_PROTECTED)
    ReadOnlyFs,
    /// Buffer too small (ERANGE / STATUS_BUFFER_TOO_SMALL)
    BufferTooSmall,
    /// No child processes (ECHILD)
    NoChild,
    /// Operation not permitted (EPERM)
    NotPermitted,
}

/// The canonical result type for all MaiOS syscall implementations.
///
/// - `Ok(value)`: syscall succeeded, `value` is the return (e.g., bytes read, PID, address)
/// - `Err(error)`: syscall failed, `error` converts to errno or NTSTATUS
pub type SyscallResult = Result<u64, SyscallError>;

impl SyscallError {
    /// Convert to a Linux negative errno value.
    pub fn to_linux_errno(self) -> i64 {
        match self {
            Self::NotPermitted      => -1,   // EPERM
            Self::NotFound          => -2,   // ENOENT
            Self::Interrupted       => -4,   // EINTR
            Self::IoError           => -5,   // EIO
            Self::NotExecutable     => -8,   // ENOEXEC
            Self::BadFileDescriptor => -9,   // EBADF
            Self::NoChild           => -10,  // ECHILD
            Self::WouldBlock        => -11,  // EAGAIN
            Self::OutOfMemory       => -12,  // ENOMEM
            Self::PermissionDenied  => -13,  // EACCES
            Self::Fault             => -14,  // EFAULT
            Self::Busy              => -16,  // EBUSY
            Self::FileExists        => -17,  // EEXIST
            Self::NoDevice          => -19,  // ENODEV
            Self::NotADirectory     => -20,  // ENOTDIR
            Self::IsADirectory      => -21,  // EISDIR
            Self::InvalidArgument   => -22,  // EINVAL
            Self::NotATerminal      => -25,  // ENOTTY
            Self::NoSpace           => -28,  // ENOSPC
            Self::IllegalSeek       => -29,  // ESPIPE
            Self::ReadOnlyFs        => -30,  // EROFS
            Self::BufferTooSmall    => -34,  // ERANGE
            Self::NotImplemented    => -38,  // ENOSYS
        }
    }

    /// Convert to a Windows NTSTATUS code (negative i32 sign-extended to i64).
    pub fn to_ntstatus(self) -> i64 {
        match self {
            Self::NotImplemented    => 0xC000_0002_u32 as i32 as i64, // STATUS_NOT_IMPLEMENTED
            Self::Fault             => 0xC000_0005_u32 as i32 as i64, // STATUS_ACCESS_VIOLATION
            Self::BadFileDescriptor => 0xC000_0008_u32 as i32 as i64, // STATUS_INVALID_HANDLE
            Self::InvalidArgument   => 0xC000_000D_u32 as i32 as i64, // STATUS_INVALID_PARAMETER
            Self::OutOfMemory       => 0xC000_0017_u32 as i32 as i64, // STATUS_NO_MEMORY
            Self::PermissionDenied  => 0xC000_0022_u32 as i32 as i64, // STATUS_ACCESS_DENIED
            Self::BufferTooSmall    => 0xC000_0023_u32 as i32 as i64, // STATUS_BUFFER_TOO_SMALL
            Self::NotFound          => 0xC000_0034_u32 as i32 as i64, // STATUS_OBJECT_NAME_NOT_FOUND
            Self::FileExists        => 0xC000_0035_u32 as i32 as i64, // STATUS_OBJECT_NAME_COLLISION
            Self::IoError           => 0xC000_0185_u32 as i32 as i64, // STATUS_IO_DEVICE_ERROR
            Self::NotExecutable     => 0xC000_0005_u32 as i32 as i64, // STATUS_INVALID_IMAGE_FORMAT
            Self::Busy              => 0xC000_0024_u32 as i32 as i64, // STATUS_OBJECT_TYPE_MISMATCH
            Self::WouldBlock        => 0x0000_0103_u32 as i32 as i64, // STATUS_PENDING
            Self::NoSpace           => 0xC000_007F_u32 as i32 as i64, // STATUS_DISK_FULL
            Self::ReadOnlyFs        => 0xC000_00A2_u32 as i32 as i64, // STATUS_MEDIA_WRITE_PROTECTED
            _                       => 0xC000_0002_u32 as i32 as i64, // fallback: NOT_IMPLEMENTED
        }
    }
}

/// Convert a `SyscallResult` to a Linux-ABI i64 return value.
///
/// - `Ok(v)` → `v as i64` (positive value)
/// - `Err(e)` → negative errno
pub fn result_to_linux(r: SyscallResult) -> i64 {
    match r {
        Ok(v) => v as i64,
        Err(e) => e.to_linux_errno(),
    }
}

/// Convert a `SyscallResult` to a Windows NT i64 return value.
///
/// - `Ok(_)` → `STATUS_SUCCESS` (0)
/// - `Err(e)` → negative NTSTATUS
///
/// Note: for NT syscalls that return data through pointer arguments,
/// the caller's adapter function writes the output before returning Ok.
pub fn result_to_ntstatus(r: SyscallResult) -> i64 {
    match r {
        Ok(_) => 0, // STATUS_SUCCESS
        Err(e) => e.to_ntstatus(),
    }
}
