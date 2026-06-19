//! macOS network I/O via kqueue event notifications.
//!
//! Implements async TCP (bind, accept, connect, read, write) and UDP (bind, send_to,
//! recv_from) operations using kqueue readiness events and non-blocking sockets,
//! including ECN-aware UDP send/receive metadata used by [`crate::cooperative_io::udp_socket`].

use std::cell::RefCell;
use std::io::{self, Read, Write};
use std::mem;
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
use std::pin::Pin;
use std::ptr;
use std::rc::Rc;

use kqueue_sys::EventFilter;
use kqueue_sys::EventFlag;
use kqueue_sys::FilterFlag;
use kqueue_sys::kevent;
use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::cooperative_io::{io_timeout::IOTimeout, udp_socket::UdpEcnCodepoint};

use super::io_wait_future::{IOCallbackWrapper, IOWaitFuture};

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
    let socket = Socket::new_raw(domain, socket_type, Some(protocol))?;

    socket.set_nonblocking(true)?;

    Ok(socket)
}

pub(crate) async fn tcp_stream_connect(
    socket_address: SocketAddr,
    timeout: &Option<IOTimeout>,
) -> io::Result<TcpStream> {
    let socket = create_socket(
        SockAddr::from(socket_address).domain(),
        Type::STREAM,
        Protocol::TCP,
    )
    .await?;

    let sock_addr = SockAddr::from(socket_address);

    let result = socket.connect(&sock_addr);

    match result {
        Ok(_) => {
            return Ok(unsafe { TcpStream::from_raw_fd(socket.into_raw_fd()) });
        }
        Err(err) if err.raw_os_error() == Some(libc::EINPROGRESS) => {
            // Connection in progress, need to wait for completion.
        }
        Err(err) => return Err(err),
    }

    let k_event = kevent {
        ident: socket.as_raw_fd() as usize,
        filter: kqueue_sys::EventFilter::EVFILT_WRITE,
        fflags: FilterFlag::empty(),
        data: 0,
        udata: ptr::null_mut(),
        flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
    };

    let buf = ptr::slice_from_raw_parts(ptr::null(), 0);

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        k_event,
        buf,
        false,
        timeout,
        IOCallbackWrapper {
            callback: move |k_event: &kevent, _: &mut [u8]| -> io::Result<usize> {
                assert_eq!(EventFilter::EVFILT_WRITE, k_event.filter);

                Ok(0)
            },
        },
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    // Pin documents that IOManager stores a pointer to this future.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let _ = pinned_io_wait_future.await?;

    if let Some(err) = socket.take_error()? {
        return Err(err);
    }

    Ok(unsafe { TcpStream::from_raw_fd(socket.into_raw_fd()) })
}

pub(crate) async fn tcp_stream_read(
    tcp_stream: &mut TcpStream,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<usize> {
    let k_event = kevent {
        ident: tcp_stream.as_raw_fd() as usize,
        filter: kqueue_sys::EventFilter::EVFILT_READ,
        fflags: FilterFlag::empty(),
        data: 0,
        udata: ptr::null_mut(),
        flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
    };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        k_event,
        buf,
        false,
        timeout,
        IOCallbackWrapper {
            callback: move |k_event: &kevent, buf: &mut [u8]| -> io::Result<usize> {
                assert_eq!(EventFilter::EVFILT_READ, k_event.filter);

                let mut tcp_stream = unsafe { TcpStream::from_raw_fd(k_event.ident as i32) };

                let result = tcp_stream.read(buf);
                let _ = tcp_stream.into_raw_fd();

                result
            },
        },
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    // Pin documents that IOManager stores a pointer to this future.
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
    let k_event = kevent {
        ident: tcp_stream.as_raw_fd() as usize,
        filter: kqueue_sys::EventFilter::EVFILT_WRITE,
        fflags: FilterFlag::empty(),
        data: 0,
        udata: ptr::null_mut(),
        flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
    };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        k_event,
        buf,
        false,
        timeout,
        IOCallbackWrapper {
            callback: move |k_event: &kevent, buf: &mut [u8]| -> io::Result<usize> {
                assert_eq!(EventFilter::EVFILT_WRITE, k_event.filter);

                let mut tcp_stream = unsafe { TcpStream::from_raw_fd(k_event.ident as i32) };

                let result = tcp_stream.write(buf);
                let _ = tcp_stream.into_raw_fd();

                result
            },
        },
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    // Pin documents that IOManager stores a pointer to this future.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}

pub(crate) async fn tcp_listener_accept(
    tcp_listener: &mut TcpListener,
    timeout: &Option<IOTimeout>,
) -> io::Result<TcpStream> {
    let k_event = kevent {
        ident: tcp_listener.as_raw_fd() as usize,
        filter: kqueue_sys::EventFilter::EVFILT_READ,
        fflags: FilterFlag::empty(),
        data: 0,
        udata: ptr::null_mut(),
        flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
    };

    let buf = ptr::slice_from_raw_parts(ptr::null(), 0);

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        k_event,
        buf,
        false,
        timeout,
        IOCallbackWrapper {
            callback: move |k_event: &kevent, _: &mut [u8]| -> io::Result<usize> {
                assert_eq!(EventFilter::EVFILT_READ, k_event.filter);

                let tcp_listener = unsafe { TcpListener::from_raw_fd(k_event.ident as i32) };

                let result = tcp_listener.accept();
                let _ = tcp_listener.into_raw_fd();

                match result {
                    Ok((tcp_stream, _)) => Ok(tcp_stream.into_raw_fd() as usize),
                    Err(err) => Err(err),
                }
            },
        },
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    // Pin documents that IOManager stores a pointer to this future.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let tcp_stream_fd = pinned_io_wait_future.await?;

    Ok(unsafe { TcpStream::from_raw_fd(tcp_stream_fd as i32) })
}

pub(crate) async fn tcp_listener_bind<A: ToSocketAddrs>(
    socket_address: A,
) -> io::Result<TcpListener> {
    let tcp_listener = TcpListener::bind(socket_address)?;

    tcp_listener.set_nonblocking(true)?;

    Ok(tcp_listener)
}

pub(crate) async fn udp_socket_bind<A: ToSocketAddrs>(socket_address: A) -> io::Result<UdpSocket> {
    let udp_socket = UdpSocket::bind(socket_address)?;

    udp_socket.set_nonblocking(true)?;

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

fn sockaddr_storage_to_socket_addr(
    storage: &libc::sockaddr_storage,
    len: libc::socklen_t,
) -> Option<SocketAddr> {
    // SAFETY: `storage` comes from the OS and `len` matches the received family.
    let sock_addr = unsafe { SockAddr::new(*storage, len) };
    sock_addr.as_socket()
}

fn parse_ecn_from_msghdr(msghdr: &mut libc::msghdr) -> Option<UdpEcnCodepoint> {
    // SAFETY: `msghdr` comes from `recvmsg`, so its control buffer is valid for
    // the duration of this ancillary-data walk.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msghdr);
        while !cmsg.is_null() {
            match ((*cmsg).cmsg_level, (*cmsg).cmsg_type) {
                (libc::IPPROTO_IP, libc::IP_TOS) => {
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

    // SAFETY: `control` is aligned for `cmsghdr`, large enough for one `c_int`
    // payload, and outlives the msghdr send operation.
    unsafe {
        msghdr.msg_control = control.as_mut_ptr();
        msghdr.msg_controllen = control.len() as _;

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

fn udp_recv_from_sync(
    raw_fd: i32,
    buf: &mut [u8],
) -> io::Result<(usize, SocketAddr, Option<UdpEcnCodepoint>)> {
    // SAFETY: sockaddr_storage is a C struct where all-zero bytes is a valid representation.
    let mut addr_storage: libc::sockaddr_storage = unsafe { mem::zeroed() };
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control = ControlMessageBuffer::new();
    // SAFETY: msghdr is a C struct where all-zero bytes is a valid representation.
    let mut msghdr: libc::msghdr = unsafe { mem::zeroed() };
    msghdr.msg_name = (&mut addr_storage as *mut libc::sockaddr_storage).cast();
    msghdr.msg_namelen = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    msghdr.msg_iov = &mut iov;
    msghdr.msg_iovlen = 1;
    msghdr.msg_control = control.as_mut_ptr();
    msghdr.msg_controllen = control.len() as _;

    // SAFETY: raw_fd is a live socket, msghdr points to valid buffers for the duration of the call.
    let received = unsafe { libc::recvmsg(raw_fd, &mut msghdr, 0) };
    if received < 0 {
        return Err(io::Error::last_os_error());
    }

    let socket_addr = sockaddr_storage_to_socket_addr(&addr_storage, msghdr.msg_namelen).ok_or(
        io::Error::new(io::ErrorKind::InvalidData, "Invalid socket address"),
    )?;
    let ecn = parse_ecn_from_msghdr(&mut msghdr);

    Ok((received as usize, socket_addr, ecn))
}

fn udp_send_to_sync(
    raw_fd: i32,
    destination_address: SocketAddr,
    buf: &mut [u8],
    ecn: Option<UdpEcnCodepoint>,
) -> io::Result<usize> {
    let destination_addr = SockAddr::from(destination_address);
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    };
    let mut control = ControlMessageBuffer::new();
    // SAFETY: msghdr is a C struct where all-zero bytes is a valid representation.
    let mut msghdr: libc::msghdr = unsafe { mem::zeroed() };
    msghdr.msg_name = destination_addr.as_ptr() as *mut _;
    msghdr.msg_namelen = destination_addr.len();
    msghdr.msg_iov = &mut iov;
    msghdr.msg_iovlen = 1;
    populate_send_ecn_msghdr(&mut msghdr, &mut control, destination_address, ecn);

    // SAFETY: raw_fd is a live socket, msghdr points to valid buffers for the duration of the call.
    let sent = unsafe { libc::sendmsg(raw_fd, &msghdr, 0) };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }

    Ok(sent as usize)
}

pub(crate) async fn udp_recv_from(
    udp_socket: &UdpSocket,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<(usize, SocketAddr, Option<UdpEcnCodepoint>)> {
    let k_event = kevent {
        ident: udp_socket.as_raw_fd() as usize,
        filter: kqueue_sys::EventFilter::EVFILT_READ,
        fflags: FilterFlag::empty(),
        data: 0,
        udata: ptr::null_mut(),
        flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
    };

    let recv_meta_cell = Rc::new(RefCell::new((SocketAddr::from(([0, 0, 0, 0], 0)), None)));
    let recv_meta_cell_clone = recv_meta_cell.clone();

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        k_event,
        buf,
        false,
        timeout,
        IOCallbackWrapper {
            callback: move |k_event: &kevent, buf: &mut [u8]| -> io::Result<usize> {
                assert_eq!(EventFilter::EVFILT_READ, k_event.filter);

                let (recv_bytes, recv_socket_addr, recv_ecn) =
                    udp_recv_from_sync(k_event.ident as i32, buf)?;
                *recv_meta_cell_clone.borrow_mut() = (recv_socket_addr, recv_ecn);

                Ok(recv_bytes)
            },
        },
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    // Pin documents that IOManager stores a pointer to this future.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    let recv_bytes = pinned_io_wait_future.await?;

    // Return the socket address captured by the callback.
    //
    let (socket_addr, ecn) = *recv_meta_cell.borrow();
    Ok((recv_bytes, socket_addr, ecn))
}

pub(crate) async fn udp_send_to(
    udp_socket: &UdpSocket,
    destination_address: SocketAddr,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
    ecn: Option<UdpEcnCodepoint>,
) -> io::Result<usize> {
    let k_event = kevent {
        ident: udp_socket.as_raw_fd() as usize,
        filter: kqueue_sys::EventFilter::EVFILT_WRITE,
        fflags: FilterFlag::empty(),
        data: 0,
        udata: ptr::null_mut(),
        flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
    };

    // Create IOWaitFuture and wait for the operation to complete.
    //
    let mut io_wait_future = IOWaitFuture::new(
        k_event,
        buf,
        false,
        timeout,
        IOCallbackWrapper {
            callback: move |k_event: &kevent, buf: &mut [u8]| -> io::Result<usize> {
                assert_eq!(EventFilter::EVFILT_WRITE, k_event.filter);
                udp_send_to_sync(k_event.ident as i32, destination_address, buf, ecn)
            },
        },
    );

    // SAFETY: `io_wait_future` is a stack local that won't be moved before the await completes.
    // Pin documents that IOManager stores a pointer to this future.
    //
    let pinned_io_wait_future: Pin<&mut IOWaitFuture> =
        unsafe { Pin::new_unchecked(&mut io_wait_future) };

    pinned_io_wait_future.await
}
