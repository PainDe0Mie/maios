//! Socket syscall stubs for MaiOS.
//!
//! MaiOS doesn't have a network stack yet. These stubs exist so that
//! programs that probe for socket support get clean ENOSYS/EAFNOSUPPORT
//! errors instead of crashing on unknown syscall numbers.

use crate::error::{SyscallResult, SyscallError};

/// EAFNOSUPPORT — address family not supported.
/// We use NotImplemented as a proxy since our error enum doesn't have EAFNOSUPPORT.

pub fn sys_socket(_domain: u64, _type_: u64, _protocol: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::NotImplemented)
}

pub fn sys_connect(_fd: u64, _addr: u64, _len: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_sendto(_fd: u64, _buf: u64, _len: u64, _flags: u64, _addr: u64, _addrlen: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_recvfrom(_fd: u64, _buf: u64, _len: u64, _flags: u64, _addr: u64, _addrlen: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_bind(_fd: u64, _addr: u64, _len: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_listen(_fd: u64, _backlog: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_accept4(_fd: u64, _addr: u64, _addrlen: u64, _flags: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_setsockopt(_fd: u64, _level: u64, _optname: u64, _optval: u64, _optlen: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_getsockopt(_fd: u64, _level: u64, _optname: u64, _optval: u64, _optlen: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_shutdown(_fd: u64, _how: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_getsockname(_fd: u64, _addr: u64, _addrlen: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_getpeername(_fd: u64, _addr: u64, _addrlen: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_socketpair(_domain: u64, _type_: u64, _protocol: u64, _sv: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::NotImplemented)
}

pub fn sys_sendmsg(_fd: u64, _msg: u64, _flags: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}

pub fn sys_recvmsg(_fd: u64, _msg: u64, _flags: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    Err(SyscallError::BadFileDescriptor)
}
