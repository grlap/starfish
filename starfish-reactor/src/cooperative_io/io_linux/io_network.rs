//! Linux network I/O via io_uring opcodes.
//!
//! Implements async TCP (bind, accept, connect, read, write) and UDP (bind, send_to,
//! recv_from) operations by submitting io_uring opcodes for non-blocking socket I/O,
//! including ECN-aware UDP send/receive metadata used by [`crate::cooperative_io::udp_socket`].

use std::io;
use std::mem;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::pin::Pin;
use std::ptr;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use std::os::unix::io::IntoRawFd;

use io_uring::{opcode, types};

use crate::cooperative_io::{io_timeout::IOTimeout, udp_socket::UdpEcnCodepoint};

use super::io_wait_future::IOWaitFuture;

#[repr(C)]
struct ControlMessageBuffer {
    bytes: [u8; 64],
    _align: [libc::cmsghdr; 0],
}

impl ControlMessageBuffer {
    fn new() -> Self {
        Self {
            bytes: [0; 64],
            _align: [],
        }
    }

    fn as_mut_ptr(&mut self) -> *mut libc::c_void {
        self.bytes.as_mut_ptr().cast()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }
}

async fn create_socket(
    domain: Domain,
    socket_type: Type,
    protocol: Protocol,
) -> io::Result<Socket> {
    //Socket::new_raw(domain, socket_type, Some(protocol))
    //Socket::new_raw(domain, socket_type, None)

    let mut io_wait_future = IOWaitFuture::new(
        opcode::Socket::new(
            i32::from(domain),
            i32::from(socket_type),
            i32::from(protocol),
        )
        .build(),
        &None,
    );

    // Pin the IOWaitFuture as IOManager::PrepareIO will store the address.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let socket_fd = pinned_io_wait_future.await?;

    Ok(unsafe { Socket::from_raw_fd(socket_fd as i32) })
}

pub(crate) async fn from_domain(domain: Domain) -> io::Result<TcpStream> {
    let socket = create_socket(domain, Type::STREAM, Protocol::TCP).await?;

    let tcp_stream = unsafe { TcpStream::from_raw_fd(socket.into_raw_fd()) };
    Ok(tcp_stream)
}

pub(crate) async fn tcp_stream_connect(
    socket_address: SocketAddr,
    timeout: &Option<IOTimeout>,
) -> io::Result<TcpStream> {
    let tcp_stream = from_domain(SockAddr::from(socket_address).domain()).await?;

    struct TcpConnectToken {
        sockaddr: SockAddr,
    }

    let mut token = TcpConnectToken {
        sockaddr: SockAddr::from(socket_address),
    };

    let token_pinned: Pin<&mut TcpConnectToken> = unsafe { Pin::new_unchecked(&mut token) };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        opcode::Connect::new(
            types::Fd(tcp_stream.as_raw_fd()),
            token_pinned.sockaddr.as_ptr() as *const _,
            token_pinned.sockaddr.len(),
        )
        .build(),
        timeout,
    );

    // Pin the IOWaitFuture as IOManager::PrepareIO will store the address.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let _ = pinned_io_wait_future.await?;

    Ok(tcp_stream)
}

pub(crate) async fn tcp_stream_read(
    tcp_stream: &mut TcpStream,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        opcode::Read::new(
            types::Fd(tcp_stream.as_raw_fd()),
            buf.as_mut_ptr(),
            buf.len() as u32,
        )
        .build(),
        timeout,
    );

    // Pin the IOWaitFuture as IOManager::PrepareIO will store the address.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}

pub(crate) async fn tcp_stream_write(
    tcp_stream: &mut TcpStream,
    buf: &[u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        opcode::Write::new(
            types::Fd(tcp_stream.as_raw_fd()),
            buf.as_ptr(),
            buf.len() as u32,
        )
        .build(),
        timeout,
    );

    // Pin the IOWaitFuture as IOManager::PrepareIO will store the address.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}

pub(crate) async fn tcp_listener_bind<A: ToSocketAddrs>(
    socket_address: A,
) -> io::Result<TcpListener> {
    let tcp_listener = TcpListener::bind(socket_address)?;

    tcp_listener.set_nonblocking(true)?;

    Ok(tcp_listener)
}

pub(crate) async fn tcp_listener_accept(
    tcp_listener: &mut TcpListener,
    timeout: &Option<IOTimeout>,
) -> io::Result<TcpStream> {
    let mut io_wait_future = IOWaitFuture::new(
        opcode::Accept::new(
            types::Fd(tcp_listener.as_raw_fd()),
            ptr::null_mut(),
            ptr::null_mut(),
        )
        .build(),
        timeout,
    );

    // Pin the IOWaitFuture as IOManager::PrepareIO will store the address.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let accept_socket_fd = pinned_io_wait_future.await?;

    let tcp_stream = unsafe { TcpStream::from_raw_fd(accept_socket_fd as i32) };
    Ok(tcp_stream)
}

pub(crate) async fn udp_socket_bind<A: ToSocketAddrs>(socket_address: A) -> io::Result<UdpSocket> {
    let udp_socket = UdpSocket::bind(socket_address)?;

    Ok(udp_socket)
}

fn ignore_unsupported_sockopt_error(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        return Ok(());
    }

    let err = io::Error::last_os_error();
    match err.raw_os_error() {
        Some(code)
            if code == libc::ENOPROTOOPT
                || code == libc::EINVAL
                || code == libc::EOPNOTSUPP
                || code == libc::ENOTSUP =>
        {
            Ok(())
        }
        _ => Err(err),
    }
}

pub(crate) fn udp_socket_enable_ecn(udp_socket: &UdpSocket) -> io::Result<()> {
    let one: libc::c_int = 1;
    let fd = udp_socket.as_raw_fd();

    // SAFETY: `fd` is a live UDP socket and the `one` pointers are valid for
    // the duration of the `setsockopt` calls.
    unsafe {
        ignore_unsupported_sockopt_error(libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_RECVTOS,
            (&one as *const libc::c_int).cast(),
            mem::size_of_val(&one) as libc::socklen_t,
        ))?;
        ignore_unsupported_sockopt_error(libc::setsockopt(
            fd,
            libc::IPPROTO_IPV6,
            libc::IPV6_RECVTCLASS,
            (&one as *const libc::c_int).cast(),
            mem::size_of_val(&one) as libc::socklen_t,
        ))?;
    }

    Ok(())
}

fn parse_ecn_from_msghdr(msghdr: &mut libc::msghdr) -> Option<UdpEcnCodepoint> {
    // SAFETY: `msghdr` comes from `recvmsg`, so its control buffer pointers are
    // initialized by the kernel for the lifetime of this walk.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msghdr);
        while !cmsg.is_null() {
            match ((*cmsg).cmsg_level, (*cmsg).cmsg_type) {
                (libc::IPPROTO_IP, libc::IP_TOS) => {
                    // The Linux kernel delivers IP_TOS as a single byte
                    // (ip_cmsg_recv_tos calls put_cmsg with len=1).
                    if (*cmsg).cmsg_len < libc::CMSG_LEN(mem::size_of::<u8>() as _) as _ {
                        return None;
                    }
                    let tos = *(libc::CMSG_DATA(cmsg) as *const u8);
                    return UdpEcnCodepoint::from_bits(tos);
                }
                (libc::IPPROTO_IPV6, libc::IPV6_TCLASS) => {
                    if (*cmsg).cmsg_len < libc::CMSG_LEN(mem::size_of::<libc::c_int>() as _) as _ {
                        return None;
                    }
                    let tclass = *(libc::CMSG_DATA(cmsg) as *const libc::c_int) as u8;
                    return UdpEcnCodepoint::from_bits(tclass);
                }
                _ => {
                    cmsg = libc::CMSG_NXTHDR(msghdr, cmsg);
                }
            }
        }
    }

    None
}

fn populate_send_ecn_msghdr(
    msghdr: &mut libc::msghdr,
    control: &mut ControlMessageBuffer,
    destination_address: SocketAddr,
    ecn: Option<UdpEcnCodepoint>,
) {
    msghdr.msg_control = ptr::null_mut();
    msghdr.msg_controllen = 0;

    let Some(ecn) = ecn else {
        return;
    };

    // SAFETY: `control` is aligned for `cmsghdr`, large enough for a single
    // `c_int` payload, and remains alive while the msghdr references it.
    unsafe {
        msghdr.msg_control = control.as_mut_ptr();
        msghdr.msg_controllen = control.len();

        let cmsg = libc::CMSG_FIRSTHDR(msghdr);
        if cmsg.is_null() {
            msghdr.msg_control = ptr::null_mut();
            msghdr.msg_controllen = 0;
            return;
        }

        (*cmsg).cmsg_level = if destination_address.is_ipv4() {
            libc::IPPROTO_IP
        } else {
            libc::IPPROTO_IPV6
        };
        (*cmsg).cmsg_type = if destination_address.is_ipv4() {
            libc::IP_TOS
        } else {
            libc::IPV6_TCLASS
        };
        (*cmsg).cmsg_len = libc::CMSG_LEN(mem::size_of::<libc::c_int>() as _) as _;
        *(libc::CMSG_DATA(cmsg) as *mut libc::c_int) = ecn.bits() as libc::c_int;
        msghdr.msg_controllen = libc::CMSG_SPACE(mem::size_of::<libc::c_int>() as _) as _;
    }
}

fn sockaddr_storage_to_socket_addr(storage: &libc::sockaddr_storage) -> Option<SocketAddr> {
    // Create a SockAddr from the sockaddr_storage.
    // First determine the length based on the address family.
    //
    let len = match storage.ss_family as libc::c_int {
        libc::AF_INET => std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        libc::AF_INET6 => std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
        libc::AF_UNIX => {
            // For Unix sockets, we need to determine the actual path length
            // This is a simplification - in practice you might need to calculate this differently.
            //
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t
        }
        // Unsupported address family.
        //
        _ => return None,
    };

    // Use socket2's SockAddr::new constructor which takes the sockaddr_storage directly.
    //
    // SAFETY: `storage` comes from the OS and `len` matches the active family.
    let sock_addr = unsafe { SockAddr::new(*storage, len) };

    // Convert to std::net::SocketAddr.
    //
    sock_addr.as_socket()
}

pub(crate) async fn udp_recv_from(
    udp_socket: &UdpSocket,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<(usize, SocketAddr, Option<UdpEcnCodepoint>)> {
    // Create a buffer to store the sender's address.
    // sockaddr_storage is large enough to hold any address type.
    //
    struct UdpRecvFromToken<'a> {
        addr_storage: libc::sockaddr_storage,
        control: ControlMessageBuffer,
        iov: libc::iovec,
        msghdr: libc::msghdr,
        buf: &'a mut [u8],
    }

    let addr_len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

    let mut token: UdpRecvFromToken = UdpRecvFromToken {
        // SAFETY: sockaddr_storage is a C struct where all-zero bytes is a valid representation.
        addr_storage: unsafe { mem::zeroed() },
        control: ControlMessageBuffer::new(),
        iov: libc::iovec {
            iov_base: ptr::null_mut(),
            iov_len: 0,
        },
        buf,
        // SAFETY: msghdr is a C struct where all-zero bytes is a valid representation.
        msghdr: unsafe { mem::zeroed() },
    };
    token.iov = libc::iovec {
        iov_base: token.buf.as_mut_ptr().cast(),
        iov_len: token.buf.len(),
    };

    // Initialize msghdr with pointers to token's fields.
    // This creates a self-referential structure, so token must not move after this.
    //
    token.msghdr = libc::msghdr {
        msg_name: &mut token.addr_storage as *mut _ as *mut libc::c_void,
        msg_namelen: addr_len,
        msg_iov: &mut token.iov,
        msg_iovlen: 1,
        msg_control: token.control.as_mut_ptr(),
        msg_controllen: token.control.len(),
        msg_flags: 0,
    };

    // SAFETY: `token` is a stack local that won't be moved before the await completes.
    // Pin documents that token is self-referential (msghdr points to addr_storage/bufs).
    //
    let mut token_pinned: Pin<&mut UdpRecvFromToken> = unsafe { Pin::new_unchecked(&mut token) };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        opcode::RecvMsg::new(types::Fd(udp_socket.as_raw_fd()), &mut token_pinned.msghdr).build(),
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    // Await the completion of the I/O operation.
    //
    let bytes_received = pinned_io_wait_future.await?;

    let sock_addr = sockaddr_storage_to_socket_addr(&token_pinned.addr_storage).ok_or(
        io::Error::new(io::ErrorKind::InvalidData, "Invalid socket address"),
    )?;
    let ecn = parse_ecn_from_msghdr(&mut token_pinned.msghdr);

    Ok((bytes_received, sock_addr, ecn))
}

pub(crate) async fn udp_send_to(
    udp_socket: &UdpSocket,
    destination_address: SocketAddr,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
    ecn: Option<UdpEcnCodepoint>,
) -> io::Result<usize> {
    struct UdpSendToToken<'a> {
        destination_addr: SockAddr,
        control: ControlMessageBuffer,
        iov: libc::iovec,
        msghdr: libc::msghdr,
        buf: &'a mut [u8],
    }

    let mut token: UdpSendToToken = UdpSendToToken {
        destination_addr: SockAddr::from(destination_address),
        control: ControlMessageBuffer::new(),
        iov: libc::iovec {
            iov_base: ptr::null_mut(),
            iov_len: 0,
        },
        buf,
        // SAFETY: msghdr is a C struct where all-zero bytes is a valid representation.
        msghdr: unsafe { mem::zeroed() },
    };
    token.iov = libc::iovec {
        iov_base: token.buf.as_mut_ptr().cast(),
        iov_len: token.buf.len(),
    };

    // Initialize msghdr with pointers to token's fields.
    // This creates a self-referential structure, so token must not move after this.
    //
    token.msghdr = libc::msghdr {
        msg_name: token.destination_addr.as_ptr() as *mut _,
        msg_namelen: token.destination_addr.len(),
        msg_iov: &mut token.iov,
        msg_iovlen: 1,
        msg_control: ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };
    populate_send_ecn_msghdr(
        &mut token.msghdr,
        &mut token.control,
        destination_address,
        ecn,
    );

    // SAFETY: `token` is a stack local that won't be moved before the await completes.
    // Pin documents that token is self-referential (msghdr points to destination_addr/bufs).
    //
    let token_pinned: Pin<&mut UdpSendToToken> = unsafe { Pin::new_unchecked(&mut token) };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        opcode::SendMsg::new(types::Fd(udp_socket.as_raw_fd()), &token_pinned.msghdr).build(),
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}
