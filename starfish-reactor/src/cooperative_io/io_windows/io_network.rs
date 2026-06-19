//! Windows network I/O via OVERLAPPED sockets.
//!
//! Implements async TCP (bind, accept, connect, read, write) and UDP (bind, send_to,
//! recv_from) operations using Windows overlapped socket APIs (AcceptEx, ConnectEx,
//! WSASend, WSARecv, etc.) for integration with the IOCP backend. The cross-platform
//! UDP API also accepts ECN parameters even though Windows currently reports no ECN
//! receive metadata and ignores outbound ECN markings in this backend.

use std::io;
use std::mem;
use std::net::Ipv4Addr;
use std::net::Ipv6Addr;
use std::net::SocketAddr;
use std::net::TcpListener;
use std::net::TcpStream;
use std::net::ToSocketAddrs;
use std::net::UdpSocket;
use std::os::windows::io::AsRawSocket;
use std::os::windows::io::FromRawSocket;
use std::os::windows::io::RawSocket;
use std::pin::Pin;
use std::ptr;

use socket2::Domain;
use socket2::Protocol;
use socket2::SockAddr;
use socket2::Socket;
use socket2::Type;
use windows::Win32::Foundation::BOOL;
use windows::Win32::Foundation::ERROR_IO_PENDING;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Networking::WinSock::AcceptEx;
use windows::Win32::Networking::WinSock::SIO_GET_EXTENSION_FUNCTION_POINTER;
use windows::Win32::Networking::WinSock::SO_UPDATE_ACCEPT_CONTEXT;
use windows::Win32::Networking::WinSock::SO_UPDATE_CONNECT_CONTEXT;
use windows::Win32::Networking::WinSock::SOCKADDR;
use windows::Win32::Networking::WinSock::SOCKADDR_IN6;
use windows::Win32::Networking::WinSock::SOCKADDR_STORAGE;
use windows::Win32::Networking::WinSock::SOCKET;
use windows::Win32::Networking::WinSock::SOL_SOCKET;
use windows::Win32::Networking::WinSock::WSABUF;
use windows::Win32::Networking::WinSock::WSAGetLastError;
use windows::Win32::Networking::WinSock::WSAID_CONNECTEX;
use windows::Win32::Networking::WinSock::WSAIoctl;
use windows::Win32::Networking::WinSock::WSARecv;
use windows::Win32::Networking::WinSock::WSARecvFrom;
use windows::Win32::Networking::WinSock::WSASend;
use windows::Win32::Networking::WinSock::WSASendTo;
use windows::Win32::Networking::WinSock::setsockopt;
use windows::Win32::System::IO::OVERLAPPED;
use windows::core::GUID;
use windows::core::PSTR;

use crate::cooperative_io::{
    io_manager::local_io_manager, io_timeout::IOTimeout, udp_socket::UdpEcnCodepoint,
};

use super::{completion_port_io_manager::CompletionPortIOManager, io_wait_future::IOWaitFuture};

const ACCEPT_EX_SOCKET_BUFFER_SIZE: u32 = mem::size_of::<SOCKADDR_STORAGE>() as u32 + 16;

const ACCEPT_EX_BUFFER_SIZE: usize = ACCEPT_EX_SOCKET_BUFFER_SIZE as usize * 2;

/// Registers a Socket Handle with IOManager.
///
fn prepare_io_with_socket(socket: &SOCKET) -> Result<(), io::Error> {
    local_io_manager::<CompletionPortIOManager>().map_or(Ok(()), |io_manager| {
        io_manager.prepare_io(HANDLE(socket.0 as isize))
    })
}

fn prepare_io_with_raw_socket(raw_socket: &RawSocket) -> Result<(), io::Error> {
    local_io_manager::<CompletionPortIOManager>().map_or(Ok(()), |io_manager| {
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
    let domain = SockAddr::from(socket_address).domain();

    let local_addr = if socket_address.is_ipv4() {
        SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0)
    } else {
        SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0)
    };

    let socket: Socket = from_domain(domain)?;

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

    let mut overlapped = OVERLAPPED::default();
    let overlapped_ptr = ptr::addr_of_mut!(overlapped);

    let mut io_wait_future =
        IOWaitFuture::new(HANDLE(raw_socket.0 as isize), overlapped_ptr, timeout);

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
            overlapped_ptr,
        )
    };

    if result == false {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::from_raw_os_error(error.0));
        }
    }

    // SAFETY: `sock_addr`, `overlapped`, and `io_wait_future` are state-machine locals
    // pinned by the async fn's Box allocation. They won't move before the await completes.
    //
    let _sock_addr_pinned: Pin<&SockAddr> = unsafe { Pin::new_unchecked(&sock_addr) };

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
    let overlapped_ptr = ptr::addr_of_mut!(overlapped);

    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(tcp_stream_raw_socket.0 as isize),
        overlapped_ptr,
        timeout,
    );

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
            Some(overlapped_ptr),
            None,
        )
    };

    if result != 0 {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::from_raw_os_error(error.0));
        }
    }

    // SAFETY: `overlapped` and `io_wait_future` are state-machine locals
    // pinned by the async fn's Box allocation. They won't move before the await completes.
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
    let overlapped_ptr = ptr::addr_of_mut!(overlapped);

    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(tcp_stream_raw_socket.0 as isize),
        overlapped_ptr,
        timeout,
    );

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
            Some(overlapped_ptr),
            None,
        )
    };

    if result != 0 {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::from_raw_os_error(error.0));
        }
    }

    // SAFETY: `overlapped` and `io_wait_future` are state-machine locals
    // pinned by the async fn's Box allocation. They won't move before the await completes.
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

    // Infer address family from the listener's local address.
    //
    let domain = match tcp_listener.local_addr()? {
        SocketAddr::V4(_) => Domain::IPV4,
        SocketAddr::V6(_) => Domain::IPV6,
    };

    // Create Accept socket matching the listener's address family.
    //
    let accept_socket = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    let raw_accept_socket = SOCKET(accept_socket.as_raw_socket() as usize);

    let mut overlapped = OVERLAPPED::default();
    let overlapped_ptr = ptr::addr_of_mut!(overlapped);

    let mut io_wait_future = IOWaitFuture::new(
        HANDLE(raw_listen_socket.0 as isize),
        overlapped_ptr,
        timeout,
    );

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
            overlapped_ptr,
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

    // SAFETY: `accept_buffer`, `overlapped`, and `io_wait_future` are state-machine locals
    // pinned by the async fn's Box allocation. They won't move before the await completes.
    //
    let _accept_buffer_pinned: Pin<&[u8; ACCEPT_EX_BUFFER_SIZE]> =
        unsafe { Pin::new_unchecked(&accept_buffer) };

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

    // Registration and ECN setup happens in UdpSocket::new() via udp_socket_enable_ecn.

    Ok(udp_socket)
}

/// Register a UDP socket with the IOCP completion port and enable ECN
/// receive metadata (IP_ECN / IPV6_ECN).
pub(crate) fn udp_socket_enable_ecn(udp_socket: &UdpSocket) -> io::Result<()> {
    // Associate the socket handle with the reactor's IOCP completion port
    // so overlapped I/O completions are delivered.
    prepare_io_with_raw_socket(&udp_socket.as_raw_socket())?;

    let raw_socket = SOCKET(udp_socket.as_raw_socket() as usize);

    // Suppress WSAECONNRESET (10054) on UDP sockets. Windows reports ICMP
    // port-unreachable as a connection-reset error on subsequent send/recv,
    // which is incorrect for connectionless UDP.
    const SIO_UDP_CONNRESET: u32 = 0x9800000C;
    let disable: u32 = 0;
    let mut bytes_returned: u32 = 0;
    // SAFETY: raw_socket is a live UDP socket, disable/bytes_returned are
    // valid for the duration of the call.
    unsafe {
        let _ = WSAIoctl(
            raw_socket,
            SIO_UDP_CONNRESET,
            Some(&disable as *const u32 as *const std::ffi::c_void),
            std::mem::size_of::<u32>() as u32,
            None,
            0,
            &mut bytes_returned,
            None,
            None,
        );
    }

    // Enable ECN codepoint reception (Windows 10 1703+).
    // IP_ECN = 50, IPV6_ECN = 50
    const IP_ECN: i32 = 50;
    const IPV6_ECN: i32 = 50;
    let one: i32 = 1;

    // SAFETY: raw_socket is a live UDP socket and `one` is valid for the
    // duration of the setsockopt calls.
    unsafe {
        // IPv4 — ignore errors on dual-stack or unsupported builds.
        let _ = setsockopt(
            raw_socket,
            windows::Win32::Networking::WinSock::IPPROTO_IP.0,
            IP_ECN,
            Some(&one.to_ne_bytes()),
        );
        // IPv6
        let _ = setsockopt(
            raw_socket,
            windows::Win32::Networking::WinSock::IPPROTO_IPV6.0,
            IPV6_ECN,
            Some(&one.to_ne_bytes()),
        );
    }

    Ok(())
}

pub(crate) async fn udp_recv_from(
    udp_socket: &UdpSocket,
    buffer: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<(usize, SocketAddr, Option<UdpEcnCodepoint>)> {
    let raw_socket = SOCKET(udp_socket.as_raw_socket() as usize);

    let mut overlapped = OVERLAPPED::default();
    let overlapped_ptr = ptr::addr_of_mut!(overlapped);

    let mut io_wait_future =
        IOWaitFuture::new(HANDLE(raw_socket.0 as isize), overlapped_ptr, timeout);

    let wsabuf = WSABUF {
        len: buffer.len() as u32,
        buf: PSTR(buffer.as_mut_ptr()),
    };
    let mut flags: u32 = 0;
    let mut sock_addr_from: SOCKADDR_IN6 = unsafe { mem::zeroed() };
    let mut sock_addr_from_len: i32 = mem::size_of::<SOCKADDR_IN6>() as i32;

    let result = unsafe {
        WSARecvFrom(
            raw_socket,
            &[wsabuf],
            None,
            &mut flags as *mut _,
            Some(&mut sock_addr_from as *mut _ as *mut SOCKADDR),
            Some(&mut sock_addr_from_len as *mut _),
            Some(overlapped_ptr),
            None,
        )
    };

    if result != 0 {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::last_os_error());
        }
    }

    // SAFETY: `sock_addr_from`, `overlapped`, and `io_wait_future` are state-machine locals
    // pinned by the async fn's Box allocation. They won't move before the await completes.
    //
    let _sock_addr_from_pinned: Pin<&SOCKADDR_IN6> = unsafe { Pin::new_unchecked(&sock_addr_from) };

    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let received = pinned_io_wait_future.await?;

    // Convert the raw sockaddr buffer to a SocketAddr via socket2.
    // SOCKADDR_IN6 is smaller than sockaddr_storage, so the transmute is safe.
    //
    let mut storage: SOCKADDR_STORAGE = unsafe { mem::zeroed() };
    unsafe {
        ptr::copy_nonoverlapping(
            &sock_addr_from as *const _ as *const u8,
            &mut storage as *mut _ as *mut u8,
            sock_addr_from_len as usize,
        );
    }
    // SAFETY: `windows::SOCKADDR_STORAGE` and socket2's internal `sockaddr_storage`
    // (from `windows-sys`) are layout-identical C structs.
    #[allow(clippy::missing_transmute_annotations)]
    let sock_addr = unsafe { SockAddr::new(mem::transmute(storage), sock_addr_from_len) };

    let socket_addr = sock_addr.as_socket().ok_or(io::Error::new(
        io::ErrorKind::InvalidData,
        "Invalid socket address",
    ))?;

    Ok((received, socket_addr, None))
}

pub(crate) async fn udp_send_to(
    udp_socket: &UdpSocket,
    socket_address: SocketAddr,
    buffer: &[u8],
    timeout: &Option<IOTimeout>,
    _ecn: Option<UdpEcnCodepoint>,
) -> io::Result<usize> {
    let raw_socket = SOCKET(udp_socket.as_raw_socket() as usize);

    let dest_addr = SockAddr::from(socket_address);

    let mut overlapped = OVERLAPPED::default();
    let overlapped_ptr = ptr::addr_of_mut!(overlapped);

    let mut io_wait_future =
        IOWaitFuture::new(HANDLE(raw_socket.0 as isize), overlapped_ptr, timeout);

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
            Some(dest_addr.as_ptr() as *const SOCKADDR),
            dest_addr.len(),
            Some(overlapped_ptr),
            None,
        )
    };

    if result != 0 {
        let error = unsafe { WSAGetLastError() };

        if error.0 != ERROR_IO_PENDING.0 as i32 {
            return Err(io::Error::from_raw_os_error(error.0));
        }
    }

    // SAFETY: `dest_addr`, `overlapped`, and `io_wait_future` are state-machine locals
    // pinned by the async fn's Box allocation. They won't move before the await completes.
    //
    let _dest_addr_pinned: Pin<&SockAddr> = unsafe { Pin::new_unchecked(&dest_addr) };

    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}
