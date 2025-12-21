use std::io;
use std::mem;
use std::net::{
    IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::os::windows::io::{AsRawSocket, FromRawSocket, RawSocket};
use std::pin::Pin;
use std::ptr;

use socket2::{Domain, Protocol, SockAddr, Socket, Type};
use windows::Win32::{
    Foundation::{BOOL, ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{
        ADDRESS_FAMILY, AF_INET, AF_INET6, AcceptEx, SIO_GET_EXTENSION_FUNCTION_POINTER,
        SO_UPDATE_ACCEPT_CONTEXT, SO_UPDATE_CONNECT_CONTEXT, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
        SOCKET, SOL_SOCKET, WSABUF, WSAGetLastError, WSAID_CONNECTEX, WSAIoctl, WSARecv,
        WSARecvFrom, WSASend, WSASendTo, setsockopt,
    },
    System::IO::OVERLAPPED,
};
use windows::core::{GUID, PSTR};

use crate::{cooperative_io::io_timeout::IOTimeout, reactor::Reactor};

use super::{completion_port_io_manager::CompletionPortIOManager, io_wait_future::IOWaitFuture};

const ACCEPT_EX_SOCKET_BUFFER_SIZE: u32 = mem::size_of::<SOCKADDR_IN>() as u32 + 16;

const ACCEPT_EX_BUFFER_SIZE: usize = ACCEPT_EX_SOCKET_BUFFER_SIZE as usize * 2;

/// Registers a Socket Handle with IOManager.
///
fn prepare_io_with_socket(socket: &SOCKET) -> Result<(), io::Error> {
    Reactor::local_instance()
        .io_manager::<CompletionPortIOManager>()
        .map_or(Ok(()), |io_manager| {
            io_manager.prepare_io(HANDLE(socket.0 as isize))
        })
}

fn prepare_io_with_raw_socket(raw_socket: &RawSocket) -> Result<(), io::Error> {
    Reactor::local_instance()
        .io_manager::<CompletionPortIOManager>()
        .map_or(Ok(()), |io_manager| {
            io_manager.prepare_io(HANDLE(*raw_socket as isize))
        })
}

fn create_socket(domain: Domain, socket_type: Type, protocol: Protocol) -> io::Result<Socket> {
    Socket::new(domain, socket_type, Some(protocol))
}

fn from_domain(domain: Domain) -> io::Result<Socket> {
    create_socket(domain, Type::STREAM, Protocol::TCP)
}

pub(crate) async fn tcp_stream_connect(
    socket_address: SocketAddr,
    timeout: &Option<IOTimeout>,
) -> io::Result<TcpStream> {
    let local_addr = SocketAddr::from(([0, 0, 0, 0], 0));

    let socket: Socket = from_domain(SockAddr::from(local_addr).domain())?;

    socket.bind(&local_addr.into())?;

    let raw_socket = SOCKET(socket.as_raw_socket() as usize);

    // Get the connectionEx function.
    //
    let mut connect_ex_fn: Option<
        unsafe extern "system" fn(
            SOCKET,
            *const SOCKADDR,
            i32,
            *const std::ffi::c_void,
            u32,
            *mut u32,
            *mut OVERLAPPED,
        ) -> BOOL,
    > = None;

    let mut connectex_guid = WSAID_CONNECTEX;
    let mut bytes: u32 = 0;

    let status = unsafe {
        WSAIoctl(
            raw_socket,
            SIO_GET_EXTENSION_FUNCTION_POINTER,
            Some(&mut connectex_guid as *mut _ as *mut std::ffi::c_void),
            mem::size_of::<GUID>() as u32,
            Some(&mut connect_ex_fn as *mut _ as *mut std::ffi::c_void),
            mem::size_of::<usize>() as u32,
            &mut bytes,
            None,
            None,
        )
    };

    if status != 0 {
        return Err(io::Error::last_os_error());
    }

    let connect_ex = connect_ex_fn.unwrap();

    // Register connected socket with IOManager (associate handle with completion port).
    //
    prepare_io_with_socket(&raw_socket)?;

    // ConnectEx.
    //
    let sock_addr = socket2::SockAddr::from(socket_address);

    // Create OVERLAPPED for the I/O operation.
    //
    let mut overlapped = OVERLAPPED::default();

    // ConnectEx without sending any data.
    //
    let result = unsafe {
        connect_ex(
            raw_socket,
            sock_addr.as_ptr() as *const SOCKADDR,
            sock_addr.len(),
            ptr::null(),
            0,
            ptr::null_mut(),
            &mut overlapped,
        )
    };

    if result == false {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::from_raw_os_error(error.0));
        }
    }

    // SAFETY: `sock_addr` and `overlapped` are stack locals that won't be moved before the await completes.
    // Pin documents that kernel holds pointers to these structures during async I/O.
    //
    let _sock_addr_pinned: Pin<&SockAddr> = unsafe { Pin::new_unchecked(&sock_addr) };
    let mut overlapped_pinned: Pin<&mut OVERLAPPED> =
        unsafe { Pin::new_unchecked(&mut overlapped) };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(raw_socket.0 as isize),
        &mut *overlapped_pinned,
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    _ = pinned_io_wait_future.await?;

    let result = unsafe { setsockopt(raw_socket, SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, None) };
    if result != 0 {
        let error = unsafe { WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(error.0));
    }

    let tcp_stream = TcpStream::from(socket);

    Ok(tcp_stream)
}

pub(crate) async fn tcp_stream_read(
    tcp_stream: &mut TcpStream,
    buffer: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    let tcp_stream_raw_socket = SOCKET(tcp_stream.as_raw_socket() as usize);

    let mut overlapped = OVERLAPPED::default();

    let wsabuf = WSABUF {
        len: buffer.len() as u32,
        buf: PSTR(buffer.as_mut_ptr()),
    };
    let mut flags: u32 = 0;

    let result = unsafe {
        WSARecv(
            tcp_stream_raw_socket,
            &[wsabuf],
            None,
            &mut flags as *mut _,
            Some(&mut overlapped),
            None,
        )
    };

    if result != 0 {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::from_raw_os_error(error.0));
        }
    }

    // SAFETY: `overlapped` is a stack local that won't be moved before the await completes.
    // Pin documents that kernel holds pointer to this structure during async I/O.
    //
    let mut overlapped_pinned: Pin<&mut OVERLAPPED> =
        unsafe { Pin::new_unchecked(&mut overlapped) };

    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(tcp_stream_raw_socket.0 as isize),
        &mut *overlapped_pinned,
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}

pub(crate) async fn tcp_stream_write(
    tcp_stream: &mut TcpStream,
    buffer: &[u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    let tcp_stream_raw_socket = SOCKET(tcp_stream.as_raw_socket() as usize);

    let mut overlapped = OVERLAPPED::default();

    let wsabuf = WSABUF {
        len: buffer.len() as u32,
        buf: PSTR(buffer.as_ptr() as *mut _),
    };

    let result = unsafe {
        WSASend(
            tcp_stream_raw_socket,
            &[wsabuf],
            None,
            0,
            Some(&mut overlapped),
            None,
        )
    };

    if result != 0 {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::from_raw_os_error(error.0));
        }
    }

    // SAFETY: `overlapped` is a stack local that won't be moved before the await completes.
    // Pin documents that kernel holds pointer to this structure during async I/O.
    //
    let mut overlapped_pinned: Pin<&mut OVERLAPPED> =
        unsafe { Pin::new_unchecked(&mut overlapped) };

    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(tcp_stream_raw_socket.0 as isize),
        &mut *overlapped_pinned,
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}

pub(crate) async fn tcp_listener_bind<A: ToSocketAddrs>(
    socket_address: A,
) -> io::Result<TcpListener> {
    let tcp_listener = TcpListener::bind(socket_address)?;

    // Register bound socket with IOManager (associate handle with completion port).
    //
    prepare_io_with_raw_socket(&tcp_listener.as_raw_socket())?;

    Ok(tcp_listener)
}

pub(crate) async fn tcp_listener_accept(
    tcp_listener: &mut TcpListener,
    timeout: &Option<IOTimeout>,
) -> io::Result<TcpStream> {
    let raw_listen_socket = SOCKET(tcp_listener.as_raw_socket() as usize);

    // Create Accept socket.
    //
    let accept_socket = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?;
    let raw_accept_socket = SOCKET(accept_socket.as_raw_socket() as usize);

    let mut overlapped = OVERLAPPED::default();

    // Buffer for AcceptEx address information.
    //
    let mut accept_buffer = [0u8; ACCEPT_EX_BUFFER_SIZE];

    let result = unsafe {
        AcceptEx(
            raw_listen_socket,
            raw_accept_socket,
            accept_buffer.as_mut_ptr() as *mut std::ffi::c_void,
            0,
            ACCEPT_EX_SOCKET_BUFFER_SIZE, // Local address size
            ACCEPT_EX_SOCKET_BUFFER_SIZE, // Remote address size
            ptr::null_mut(),              // Bytes received
            &mut overlapped,
        )
    };

    // AcceptEx returns TRUE on immediate success, FALSE on error/pending.
    //
    if result == false {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::from_raw_os_error(error.0));
        }
    }

    // SAFETY: `overlapped` and `accept_buffer` are stack locals that won't be moved before the await completes.
    // Pin documents that kernel holds pointers to these structures during async I/O.
    //
    let _accept_buffer_pinned: Pin<&[u8; ACCEPT_EX_BUFFER_SIZE]> =
        unsafe { Pin::new_unchecked(&accept_buffer) };
    let mut overlapped_pinned: Pin<&mut OVERLAPPED> =
        unsafe { Pin::new_unchecked(&mut overlapped) };

    // Wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(raw_listen_socket.0 as isize),
        &mut *overlapped_pinned,
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let _ = pinned_io_wait_future.await?;

    let listen_socket_bytes = raw_listen_socket.0.to_ne_bytes();

    let result = unsafe {
        setsockopt(
            raw_accept_socket,
            SOL_SOCKET,
            SO_UPDATE_ACCEPT_CONTEXT,
            Some(&listen_socket_bytes),
        )
    };

    if result != 0 {
        let error = unsafe { WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(error.0));
    }

    // Register accepted socket with IOManager (associate handle with completion port).
    //
    prepare_io_with_socket(&raw_accept_socket)?;

    // Take the ownership of the raw_accept_socket.
    //
    let tcp_stream = unsafe { TcpStream::from_raw_socket(raw_accept_socket.0 as u64) };
    std::mem::forget(accept_socket);

    Ok(tcp_stream)
}

pub(crate) async fn udp_socket_bind<A: ToSocketAddrs>(socket_address: A) -> io::Result<UdpSocket> {
    let udp_socket = UdpSocket::bind(socket_address)?;

    // Register bound socket with IOManager (associate handle with completion port).
    //
    prepare_io_with_raw_socket(&udp_socket.as_raw_socket())?;

    Ok(udp_socket)
}

fn sockaddr_to_socket_addr(sockaddr: &SOCKADDR) -> Option<SocketAddr> {
    // Get the address family (first field in any socket address structure)
    let family = unsafe { *(sockaddr as *const _ as *const ADDRESS_FAMILY) };

    match family {
        AF_INET => {
            // It's an IPv4 address
            let addr_in = unsafe { &*(sockaddr as *const _ as *const SOCKADDR_IN) };

            // Get IP address (stored in network byte order)
            let ip_addr = unsafe { addr_in.sin_addr.S_un.S_addr };
            let ip_bytes = [
                (ip_addr & 0xFF) as u8,
                ((ip_addr >> 8) & 0xFF) as u8,
                ((ip_addr >> 16) & 0xFF) as u8,
                ((ip_addr >> 24) & 0xFF) as u8,
            ];

            // Get port (also in network byte order)
            let port = u16::from_be(addr_in.sin_port);

            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip_bytes)), port))
        }

        AF_INET6 => {
            // It's an IPv6 address
            let addr_in6 = unsafe { &*(sockaddr as *const _ as *const SOCKADDR_IN6) };

            // Get IPv6 address bytes
            let ip_bytes = unsafe { addr_in6.sin6_addr.u.Byte };

            // Get port (in network byte order)
            let port = u16::from_be(addr_in6.sin6_port);

            // Create IPv6 address
            let ipv6 = Ipv6Addr::from(ip_bytes);
            let mut socket_addr = SocketAddr::new(IpAddr::V6(ipv6), port);

            // Set additional IPv6 fields
            if let SocketAddr::V6(ref mut addr_v6) = socket_addr {
                addr_v6.set_flowinfo(addr_in6.sin6_flowinfo);
            }

            Some(socket_addr)
        }

        _ => None, // Unsupported address family
    }
}

pub(crate) async fn udp_recv_from(
    udp_socket: &UdpSocket,
    buffer: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<(usize, SocketAddr)> {
    let raw_socket = SOCKET(udp_socket.as_raw_socket() as usize);

    let mut overlapped = OVERLAPPED::default();

    let wsabuf = WSABUF {
        len: buffer.len() as u32,
        buf: PSTR(buffer.as_mut_ptr()),
    };
    let mut flags: u32 = 0;
    let mut sock_addr_from = SOCKADDR::default();
    let mut sock_addr_from_len: i32 = std::mem::size_of::<SOCKADDR>() as i32;

    let result = unsafe {
        WSARecvFrom(
            raw_socket,
            &[wsabuf],
            None,
            &mut flags as *mut _,
            Some(&mut sock_addr_from as *mut _),
            Some(&mut sock_addr_from_len as *mut _),
            Some(&mut overlapped),
            None,
        )
    };

    if result != 0 {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::last_os_error());
        }
    }

    // SAFETY: `overlapped` and `sock_addr_from` are stack locals that won't be moved before the await completes.
    // Pin documents that kernel holds pointers to these structures during async I/O.
    //
    let _sock_addr_from_pinned: Pin<&SOCKADDR> = unsafe { Pin::new_unchecked(&sock_addr_from) };
    let mut overlapped_pinned: Pin<&mut OVERLAPPED> =
        unsafe { Pin::new_unchecked(&mut overlapped) };

    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(raw_socket.0 as isize),
        &mut *overlapped_pinned,
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let received = pinned_io_wait_future.await?;

    // Create a SockAddr from the raw sockaddr pointer and length
    //
    let sock_addr = sockaddr_to_socket_addr(&sock_addr_from).ok_or(io::Error::new(
        io::ErrorKind::InvalidData,
        "Invalid socket address",
    ))?;

    Ok((received, sock_addr))
}

pub(crate) async fn udp_send_to(
    udp_socket: &UdpSocket,
    socket_address: SocketAddr,
    buffer: &[u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    let raw_socket = SOCKET(udp_socket.as_raw_socket() as usize);

    let mut sock_addr_in = SOCKADDR_IN::default();
    if let SocketAddr::V4(v4) = socket_address {
        sock_addr_in.sin_family = AF_INET;
        sock_addr_in.sin_port = v4.port().to_be();
        let ip_bytes = v4.ip().octets();
        sock_addr_in.sin_addr.S_un.S_addr = u32::from_ne_bytes(ip_bytes);
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Only IPv4 is supported",
        ));
    }

    let mut overlapped = OVERLAPPED::default();

    let wsabuf = WSABUF {
        len: buffer.len() as u32,
        buf: PSTR(buffer.as_ptr() as *mut _),
    };

    let result = unsafe {
        WSASendTo(
            raw_socket,
            &[wsabuf],
            None,
            0,
            Some(&sock_addr_in as *const _ as *const SOCKADDR),
            size_of::<SOCKADDR_IN>() as i32,
            Some(&mut overlapped),
            None,
        )
    };

    if result != 0 {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::from_raw_os_error(error.0));
        }
    }

    // SAFETY: `overlapped` and `sock_addr_in` are stack locals that won't be moved before the await completes.
    // Pin documents that kernel holds pointers to these structures during async I/O.
    //
    let _sock_addr_in_pinned: Pin<&SOCKADDR_IN> = unsafe { Pin::new_unchecked(&sock_addr_in) };
    let mut overlapped_pinned: Pin<&mut OVERLAPPED> =
        unsafe { Pin::new_unchecked(&mut overlapped) };

    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(raw_socket.0 as isize),
        &mut *overlapped_pinned,
        timeout,
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}
