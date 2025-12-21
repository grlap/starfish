use std::io;
use std::mem;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::pin::Pin;
use std::ptr;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use std::os::unix::io::IntoRawFd;

use io_uring::{opcode, types};

use crate::cooperative_io::io_timeout::IOTimeout;

use super::io_wait_future::IOWaitFuture;

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
    let sock_addr = unsafe { SockAddr::new(*storage, len) };

    // Convert to std::net::SocketAddr.
    //
    sock_addr.as_socket()
}

pub(crate) async fn udp_recv_from(
    udp_socket: &UdpSocket,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<(usize, SocketAddr)> {
    // Create a buffer to store the sender's address.
    // sockaddr_storage is large enough to hold any address type.
    //
    struct UdpRecvFromToken<'a> {
        addr_storage: libc::sockaddr_storage,
        msghdr: libc::msghdr,
        bufs: [&'a mut [u8]; 1],
    }

    let addr_len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;

    let mut token: UdpRecvFromToken = UdpRecvFromToken {
        addr_storage: unsafe { mem::zeroed() },
        bufs: [buf],
        msghdr: unsafe { mem::zeroed() },
    };

    // Initialize msghdr with pointers to token's fields.
    // This creates a self-referential structure, so token must not move after this.
    //
    token.msghdr = libc::msghdr {
        msg_name: &mut token.addr_storage as *mut _ as *mut libc::c_void,
        msg_namelen: addr_len,
        msg_iov: token.bufs.as_mut_ptr().cast(),
        msg_iovlen: token.bufs.len() as _,
        msg_control: ptr::null_mut(),
        msg_controllen: 0,
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

    Ok((bytes_received, sock_addr))
}

pub(crate) async fn udp_send_to(
    udp_socket: &UdpSocket,
    destination_address: SocketAddr,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    struct UdpSendToToken<'a> {
        destination_addr: SockAddr,
        msghdr: libc::msghdr,
        bufs: [&'a mut [u8]; 1],
    }

    let mut token: UdpSendToToken = UdpSendToToken {
        destination_addr: SockAddr::from(destination_address),
        bufs: [buf],
        msghdr: unsafe { mem::zeroed() },
    };

    // Initialize msghdr with pointers to token's fields.
    // This creates a self-referential structure, so token must not move after this.
    //
    token.msghdr = libc::msghdr {
        msg_name: token.destination_addr.as_ptr() as *mut _,
        msg_namelen: token.destination_addr.len(),
        msg_iov: token.bufs.as_mut_ptr().cast(),
        msg_iovlen: token.bufs.len() as _,
        msg_control: ptr::null_mut(),
        msg_controllen: 0,
        msg_flags: 0,
    };

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
