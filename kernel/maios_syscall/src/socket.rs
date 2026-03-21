//! Socket syscalls for MaiOS — real implementation backed by smoltcp.
//!
//! Supports AF_INET (IPv4) with SOCK_STREAM (TCP) and SOCK_DGRAM (UDP).
//! Sockets are stored in the per-task ResourceTable as `Resource::Socket`.

use crate::error::{SyscallResult, SyscallError};
use crate::resource::{self, Resource, SocketKind};
use alloc::sync::Arc;
use net::NetworkInterface;
use smoltcp::socket::{tcp, udp};
use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

// Linux constants
const AF_INET: u64 = 2;
const SOCK_STREAM: u64 = 1;
const SOCK_DGRAM: u64 = 2;
const SOCK_NONBLOCK: u64 = 0o4000;
const SOCK_CLOEXEC: u64 = 0o2000000;

// Mask for socket type (remove flags)
const SOCK_TYPE_MASK: u64 = 0xF;

/// Maximum number of poll iterations for blocking operations.
const MAX_POLL_ITERS: usize = 500_000;

fn current_task_id() -> usize {
    task::get_my_current_task_id()
}

fn get_interface() -> Result<Arc<NetworkInterface>, SyscallError> {
    net::get_default_interface().ok_or(SyscallError::NetworkUnreachable)
}

/// Parse a Linux `struct sockaddr_in` from userspace memory.
unsafe fn parse_sockaddr_in(addr_ptr: u64, addr_len: u64) -> Result<IpEndpoint, SyscallError> {
    if addr_ptr == 0 || addr_len < 8 {
        return Err(SyscallError::InvalidArgument);
    }
    let ptr = addr_ptr as *const u8;
    let family = u16::from_ne_bytes([*ptr, *ptr.add(1)]);
    if family != AF_INET as u16 {
        return Err(SyscallError::InvalidArgument);
    }
    // Port is in network byte order (big-endian)
    let port = u16::from_be_bytes([*ptr.add(2), *ptr.add(3)]);
    // IPv4 address is 4 bytes at offset 4, in network byte order
    let ip = Ipv4Address::new(*ptr.add(4), *ptr.add(5), *ptr.add(6), *ptr.add(7));
    Ok(IpEndpoint::new(IpAddress::Ipv4(ip), port))
}

/// Write a `struct sockaddr_in` to userspace memory.
unsafe fn write_sockaddr_in(addr_ptr: u64, addrlen_ptr: u64, endpoint: IpEndpoint) -> Result<(), SyscallError> {
    if addr_ptr == 0 {
        return Ok(());
    }
    let ptr = addr_ptr as *mut u8;
    // sin_family = AF_INET
    let family_bytes = (AF_INET as u16).to_ne_bytes();
    *ptr = family_bytes[0];
    *ptr.add(1) = family_bytes[1];
    // sin_port (network byte order)
    let port_bytes = endpoint.port.to_be_bytes();
    *ptr.add(2) = port_bytes[0];
    *ptr.add(3) = port_bytes[1];
    // sin_addr
    if let IpAddress::Ipv4(ipv4) = endpoint.addr {
        let octets = ipv4.0;
        *ptr.add(4) = octets[0];
        *ptr.add(5) = octets[1];
        *ptr.add(6) = octets[2];
        *ptr.add(7) = octets[3];
    }
    // sin_zero
    for i in 8..16 {
        *ptr.add(i) = 0;
    }
    // Write addrlen
    if addrlen_ptr != 0 {
        let len_ptr = addrlen_ptr as *mut u32;
        *len_ptr = 16; // sizeof(sockaddr_in)
    }
    Ok(())
}

/// Poll the interface, yielding between attempts, until a condition is met.
fn poll_until<F>(iface: &Arc<NetworkInterface>, mut check: F) -> Result<(), SyscallError>
where
    F: FnMut(&Arc<NetworkInterface>) -> Option<Result<(), SyscallError>>,
{
    for _ in 0..MAX_POLL_ITERS {
        iface.poll();
        if let Some(result) = check(iface) {
            return result;
        }
        // Yield to other tasks
        task::schedule();
    }
    Err(SyscallError::WouldBlock)
}

// ─── sys_socket ──────────────────────────────────────────────────────────────

pub fn sys_socket(domain: u64, type_: u64, _protocol: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if domain != AF_INET {
        return Err(SyscallError::InvalidArgument);
    }

    let sock_type = type_ & SOCK_TYPE_MASK;
    let iface = get_interface()?;

    let (handle, kind) = match sock_type {
        SOCK_STREAM => {
            let tcp_rx = tcp::Socket::new(
                smoltcp::socket::tcp::SocketBuffer::new(alloc::vec![0u8; 65535]),
                smoltcp::socket::tcp::SocketBuffer::new(alloc::vec![0u8; 65535]),
            );
            let handle = iface.sockets.lock().add(tcp_rx);
            (handle, SocketKind::Tcp)
        }
        SOCK_DGRAM => {
            let udp_rx = udp::Socket::new(
                smoltcp::socket::udp::PacketBuffer::new(
                    alloc::vec![smoltcp::socket::udp::PacketMetadata::EMPTY; 16],
                    alloc::vec![0u8; 65535],
                ),
                smoltcp::socket::udp::PacketBuffer::new(
                    alloc::vec![smoltcp::socket::udp::PacketMetadata::EMPTY; 16],
                    alloc::vec![0u8; 65535],
                ),
            );
            let handle = iface.sockets.lock().add(udp_rx);
            (handle, SocketKind::Udp)
        }
        _ => return Err(SyscallError::InvalidArgument),
    };

    let tid = current_task_id();
    let fd = resource::with_resources_mut(tid, |table| {
        table.alloc_fd(Resource::Socket {
            handle,
            interface: iface,
            sock_type: kind,
        })
    });
    Ok(fd)
}

// ─── sys_connect ─────────────────────────────────────────────────────────────

pub fn sys_connect(fd: u64, addr: u64, len: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let endpoint = unsafe { parse_sockaddr_in(addr, len)? };
    let tid = current_task_id();

    let (handle, iface, kind) = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { handle, interface, sock_type }) => {
                Ok((*handle, interface.clone(), *sock_type))
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })?;

    match kind {
        SocketKind::Tcp => {
            // Initiate TCP connection
            {
                let mut sockets = iface.sockets.lock();
                let tcp_sock: &mut tcp::Socket = sockets.get_mut(handle);
                let local_port = net::get_ephemeral_port();
                let mut inner = iface.inner.lock();
                let cx = inner.context();
                tcp_sock.connect(cx, endpoint, local_port)
                    .map_err(|_| SyscallError::ConnectionRefused)?;
            }

            // Poll until connected or error
            poll_until(&iface, |iface| {
                let sockets = iface.sockets.lock();
                let tcp_sock: &tcp::Socket = sockets.get(handle);
                match tcp_sock.state() {
                    tcp::State::Established => Some(Ok(())),
                    tcp::State::Closed | tcp::State::TimeWait => {
                        Some(Err(SyscallError::ConnectionRefused))
                    }
                    _ => None, // Still connecting
                }
            })?;
            Ok(0)
        }
        SocketKind::Udp => {
            // UDP "connect" just remembers the remote endpoint — no actual connection.
            // smoltcp UDP doesn't have a connect concept, so this is a no-op success.
            // The endpoint will be used as default for send().
            Ok(0)
        }
    }
}

// ─── sys_bind ────────────────────────────────────────────────────────────────

pub fn sys_bind(fd: u64, addr: u64, len: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let endpoint = unsafe { parse_sockaddr_in(addr, len)? };
    let tid = current_task_id();

    let (handle, iface, kind) = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { handle, interface, sock_type }) => {
                Ok((*handle, interface.clone(), *sock_type))
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })?;

    match kind {
        SocketKind::Tcp => {
            // TCP bind is implicit — listen() will use the endpoint
            Ok(0)
        }
        SocketKind::Udp => {
            let mut sockets = iface.sockets.lock();
            let udp_sock: &mut udp::Socket = sockets.get_mut(handle);
            let listen_ep = if endpoint.addr.is_unspecified() {
                IpListenEndpoint { addr: None, port: endpoint.port }
            } else {
                IpListenEndpoint { addr: Some(endpoint.addr), port: endpoint.port }
            };
            udp_sock.bind(listen_ep).map_err(|_| SyscallError::AddressInUse)?;
            Ok(0)
        }
    }
}

// ─── sys_listen ──────────────────────────────────────────────────────────────

pub fn sys_listen(fd: u64, _backlog: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let tid = current_task_id();

    let (handle, iface, kind) = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { handle, interface, sock_type }) => {
                Ok((*handle, interface.clone(), *sock_type))
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })?;

    if kind != SocketKind::Tcp {
        return Err(SyscallError::InvalidArgument);
    }

    let mut sockets = iface.sockets.lock();
    let tcp_sock: &mut tcp::Socket = sockets.get_mut(handle);
    // smoltcp listen takes a local endpoint
    let local_port = net::get_ephemeral_port();
    tcp_sock.listen(local_port).map_err(|_| SyscallError::AddressInUse)?;
    Ok(0)
}

// ─── sys_accept4 ─────────────────────────────────────────────────────────────

pub fn sys_accept4(fd: u64, addr: u64, addrlen: u64, _flags: u64, _: u64, _: u64) -> SyscallResult {
    let tid = current_task_id();

    let (handle, iface, kind) = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { handle, interface, sock_type }) => {
                Ok((*handle, interface.clone(), *sock_type))
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })?;

    if kind != SocketKind::Tcp {
        return Err(SyscallError::InvalidArgument);
    }

    // Wait for an incoming connection
    poll_until(&iface, |iface| {
        let sockets = iface.sockets.lock();
        let tcp_sock: &tcp::Socket = sockets.get(handle);
        if tcp_sock.is_active() {
            Some(Ok(()))
        } else {
            None
        }
    })?;

    // Fill in the remote address if requested
    if addr != 0 {
        let sockets = iface.sockets.lock();
        let tcp_sock: &tcp::Socket = sockets.get(handle);
        if let Some(remote) = tcp_sock.remote_endpoint() {
            unsafe { write_sockaddr_in(addr, addrlen, remote)?; }
        }
    }

    // Create a new socket for the accepted connection and return its fd.
    // smoltcp doesn't have a separate accept model — the listening socket
    // becomes the connected socket. We create a new listening socket to
    // replace it, and return the existing handle as the "accepted" fd.
    let new_tcp = tcp::Socket::new(
        smoltcp::socket::tcp::SocketBuffer::new(alloc::vec![0u8; 65535]),
        smoltcp::socket::tcp::SocketBuffer::new(alloc::vec![0u8; 65535]),
    );
    let new_handle = iface.sockets.lock().add(new_tcp);

    // The old handle is now the connected socket; new_handle is the new listener.
    // Swap: give caller the connected socket, put new listener back on the fd.
    let new_fd = resource::with_resources_mut(tid, |table| {
        // Allocate fd for the connected socket (old handle)
        let accepted_fd = table.alloc_fd(Resource::Socket {
            handle,
            interface: iface.clone(),
            sock_type: SocketKind::Tcp,
        });
        accepted_fd
    });

    // Update the listening fd to point to the new (empty) socket
    resource::with_resources_mut(tid, |table| {
        if let Some(Resource::Socket { handle: ref mut h, .. }) = table.get_mut(fd) {
            *h = new_handle;
        }
    });

    Ok(new_fd)
}

// ─── sys_sendto ──────────────────────────────────────────────────────────────

pub fn sys_sendto(fd: u64, buf: u64, len: u64, _flags: u64, dest_addr: u64, addrlen: u64) -> SyscallResult {
    if buf == 0 || len == 0 {
        return Ok(0);
    }
    let data = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };
    let tid = current_task_id();

    let (handle, iface, kind) = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { handle, interface, sock_type }) => {
                Ok((*handle, interface.clone(), *sock_type))
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })?;

    match kind {
        SocketKind::Tcp => {
            let mut sent = 0usize;
            for _ in 0..MAX_POLL_ITERS {
                iface.poll();
                let mut sockets = iface.sockets.lock();
                let tcp_sock: &mut tcp::Socket = sockets.get_mut(handle);
                if !tcp_sock.is_active() {
                    return Err(SyscallError::ConnectionReset);
                }
                if tcp_sock.can_send() {
                    match tcp_sock.send_slice(&data[sent..]) {
                        Ok(n) => {
                            sent += n;
                            if sent >= data.len() {
                                drop(sockets);
                                iface.poll();
                                return Ok(sent as u64);
                            }
                        }
                        Err(_) => return Err(SyscallError::IoError),
                    }
                }
                drop(sockets);
                task::schedule();
            }
            if sent > 0 { Ok(sent as u64) } else { Err(SyscallError::WouldBlock) }
        }
        SocketKind::Udp => {
            let remote = if dest_addr != 0 {
                unsafe { parse_sockaddr_in(dest_addr, addrlen)? }
            } else {
                return Err(SyscallError::InvalidArgument);
            };

            // Ensure the socket is bound
            {
                let mut sockets = iface.sockets.lock();
                let udp_sock: &mut udp::Socket = sockets.get_mut(handle);
                if !udp_sock.is_open() {
                    let port = net::get_ephemeral_port();
                    udp_sock.bind(port).map_err(|_| SyscallError::AddressInUse)?;
                }
            }

            for _ in 0..MAX_POLL_ITERS {
                iface.poll();
                let mut sockets = iface.sockets.lock();
                let udp_sock: &mut udp::Socket = sockets.get_mut(handle);
                if udp_sock.can_send() {
                    udp_sock.send_slice(data, remote).map_err(|_| SyscallError::IoError)?;
                    drop(sockets);
                    iface.poll();
                    return Ok(len);
                }
                drop(sockets);
                task::schedule();
            }
            Err(SyscallError::WouldBlock)
        }
    }
}

// ─── sys_recvfrom ────────────────────────────────────────────────────────────

pub fn sys_recvfrom(fd: u64, buf: u64, len: u64, _flags: u64, src_addr: u64, addrlen: u64) -> SyscallResult {
    if buf == 0 || len == 0 {
        return Ok(0);
    }
    let buffer = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len as usize) };
    let tid = current_task_id();

    let (handle, iface, kind) = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { handle, interface, sock_type }) => {
                Ok((*handle, interface.clone(), *sock_type))
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })?;

    match kind {
        SocketKind::Tcp => {
            for _ in 0..MAX_POLL_ITERS {
                iface.poll();
                let mut sockets = iface.sockets.lock();
                let tcp_sock: &mut tcp::Socket = sockets.get_mut(handle);
                if tcp_sock.can_recv() {
                    let n = tcp_sock.recv_slice(buffer).map_err(|_| SyscallError::IoError)?;
                    return Ok(n as u64);
                }
                if !tcp_sock.is_active() {
                    // Connection closed — return 0 (EOF)
                    return Ok(0);
                }
                drop(sockets);
                task::schedule();
            }
            Err(SyscallError::WouldBlock)
        }
        SocketKind::Udp => {
            for _ in 0..MAX_POLL_ITERS {
                iface.poll();
                let mut sockets = iface.sockets.lock();
                let udp_sock: &mut udp::Socket = sockets.get_mut(handle);
                if udp_sock.can_recv() {
                    match udp_sock.recv_slice(buffer) {
                        Ok((n, meta)) => {
                            if src_addr != 0 {
                                unsafe { write_sockaddr_in(src_addr, addrlen, meta.endpoint)?; }
                            }
                            return Ok(n as u64);
                        }
                        Err(_) => return Err(SyscallError::IoError),
                    }
                }
                drop(sockets);
                task::schedule();
            }
            Err(SyscallError::WouldBlock)
        }
    }
}

// ─── sys_setsockopt / sys_getsockopt ─────────────────────────────────────────

pub fn sys_setsockopt(_fd: u64, _level: u64, _optname: u64, _optval: u64, _optlen: u64, _: u64) -> SyscallResult {
    // Accept silently — most options are no-ops for smoltcp
    Ok(0)
}

pub fn sys_getsockopt(fd: u64, _level: u64, _optname: u64, optval: u64, optlen: u64, _: u64) -> SyscallResult {
    let tid = current_task_id();
    resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { .. }) => {
                // Return 0 (success) as default option value
                if optval != 0 && optlen != 0 {
                    unsafe {
                        let len_ptr = optlen as *mut u32;
                        let val_ptr = optval as *mut u32;
                        if *len_ptr >= 4 {
                            *val_ptr = 0;
                            *len_ptr = 4;
                        }
                    }
                }
                Ok(0)
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })
}

// ─── sys_shutdown ────────────────────────────────────────────────────────────

pub fn sys_shutdown(fd: u64, how: u64, _: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let tid = current_task_id();

    let (handle, iface, kind) = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { handle, interface, sock_type }) => {
                Ok((*handle, interface.clone(), *sock_type))
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })?;

    if kind == SocketKind::Tcp {
        let mut sockets = iface.sockets.lock();
        let tcp_sock: &mut tcp::Socket = sockets.get_mut(handle);
        match how {
            0 => { /* SHUT_RD — no-op in smoltcp */ }
            1 | 2 => { tcp_sock.close(); }
            _ => return Err(SyscallError::InvalidArgument),
        }
    }
    Ok(0)
}

// ─── sys_getsockname / sys_getpeername ───────────────────────────────────────

pub fn sys_getsockname(fd: u64, addr: u64, addrlen: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let tid = current_task_id();

    let (handle, iface, kind) = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { handle, interface, sock_type }) => {
                Ok((*handle, interface.clone(), *sock_type))
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })?;

    let endpoint = match kind {
        SocketKind::Tcp => {
            let sockets = iface.sockets.lock();
            let tcp_sock: &tcp::Socket = sockets.get(handle);
            tcp_sock.local_endpoint().unwrap_or(IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED), 0))
        }
        SocketKind::Udp => {
            let sockets = iface.sockets.lock();
            let udp_sock: &udp::Socket = sockets.get(handle);
            let ep = udp_sock.endpoint();
            IpEndpoint::new(ep.addr.unwrap_or(IpAddress::Ipv4(Ipv4Address::UNSPECIFIED)), ep.port)
        }
    };

    unsafe { write_sockaddr_in(addr, addrlen, endpoint)?; }
    Ok(0)
}

pub fn sys_getpeername(fd: u64, addr: u64, addrlen: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    let tid = current_task_id();

    let (handle, iface, kind) = resource::with_resources(tid, |table| {
        match table.get(fd) {
            Some(Resource::Socket { handle, interface, sock_type }) => {
                Ok((*handle, interface.clone(), *sock_type))
            }
            _ => Err(SyscallError::BadFileDescriptor),
        }
    })?;

    if kind != SocketKind::Tcp {
        return Err(SyscallError::InvalidArgument);
    }

    let sockets = iface.sockets.lock();
    let tcp_sock: &tcp::Socket = sockets.get(handle);
    let endpoint = tcp_sock.remote_endpoint().ok_or(SyscallError::InvalidArgument)?;
    drop(sockets);

    unsafe { write_sockaddr_in(addr, addrlen, endpoint)?; }
    Ok(0)
}

// ─── sys_socketpair ──────────────────────────────────────────────────────────

pub fn sys_socketpair(_domain: u64, _type_: u64, _protocol: u64, _sv: u64, _: u64, _: u64) -> SyscallResult {
    // AF_UNIX socketpair — not supported yet
    Err(SyscallError::NotImplemented)
}

// ─── sys_sendmsg / sys_recvmsg ───────────────────────────────────────────────

pub fn sys_sendmsg(fd: u64, msg: u64, flags: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    // Simplified: extract iov from msghdr and call sendto
    if msg == 0 {
        return Err(SyscallError::InvalidArgument);
    }
    unsafe {
        let msghdr = msg as *const u8;
        // struct msghdr { msg_name, msg_namelen, msg_iov, msg_iovlen, ... }
        // offsets: name=0, namelen=8, iov=16, iovlen=24
        let iov_ptr = *(msghdr.add(16) as *const u64);
        let iov_len = *(msghdr.add(24) as *const u64);
        let dest_addr = *(msghdr as *const u64);
        let dest_len = *(msghdr.add(8) as *const u32) as u64;

        let mut total = 0u64;
        for i in 0..iov_len {
            let iov = (iov_ptr as *const u8).add(i as usize * 16);
            let base = *(iov as *const u64);
            let len = *(iov.add(8) as *const u64);
            let sent = sys_sendto(fd, base, len, flags, dest_addr, dest_len)?;
            total += sent;
        }
        Ok(total)
    }
}

pub fn sys_recvmsg(fd: u64, msg: u64, flags: u64, _: u64, _: u64, _: u64) -> SyscallResult {
    if msg == 0 {
        return Err(SyscallError::InvalidArgument);
    }
    unsafe {
        let msghdr = msg as *const u8;
        let iov_ptr = *(msghdr.add(16) as *const u64);
        let iov_len = *(msghdr.add(24) as *const u64);
        let src_addr = *(msghdr as *const u64);
        let src_len_ptr = msghdr.add(8) as u64;

        let mut total = 0u64;
        for i in 0..iov_len {
            let iov = (iov_ptr as *const u8).add(i as usize * 16);
            let base = *(iov as *const u64);
            let len = *(iov.add(8) as *const u64);
            let received = sys_recvfrom(fd, base, len, flags, src_addr, src_len_ptr)?;
            total += received;
            if (received as u64) < len {
                break; // Short read
            }
        }
        Ok(total)
    }
}
