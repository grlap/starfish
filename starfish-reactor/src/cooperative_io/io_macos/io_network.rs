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

use crate::cooperative_io::io_timeout::IOTimeout;

use super::io_wait_future::{IOCallbackWrapper, IOWaitFuture};

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

    // TODO: check the connection status with getsockopt(SO_ERROR).
    //

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

pub(crate) async fn udp_recv_from(
    udp_socket: &UdpSocket,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
) -> io::Result<(usize, SocketAddr)> {
    let k_event = kevent {
        ident: udp_socket.as_raw_fd() as usize,
        filter: kqueue_sys::EventFilter::EVFILT_READ,
        fflags: FilterFlag::empty(),
        data: 0,
        udata: ptr::null_mut(),
        flags: EventFlag::EV_ADD | EventFlag::EV_ENABLE | EventFlag::EV_CLEAR,
    };

    let socket_addr: SocketAddr = unsafe { mem::zeroed() };

    let socket_addr_cell = Rc::new(RefCell::new(socket_addr));
    let socket_addr_cell_clone = socket_addr_cell.clone();

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

                let socket = unsafe { UdpSocket::from_raw_fd(k_event.ident as i32) };

                let (recv_bytes, recv_socket_addr) = socket.recv_from(buf)?;
                let _ = socket.into_raw_fd();
                *socket_addr_cell_clone.borrow_mut() = recv_socket_addr;

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
    Ok((recv_bytes, *socket_addr_cell.borrow()))
}

pub(crate) async fn udp_send_to(
    udp_socket: &UdpSocket,
    destination_address: SocketAddr,
    buf: &mut [u8],
    timeout: &Option<IOTimeout>,
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

                let socket = unsafe { UdpSocket::from_raw_fd(k_event.ident as i32) };

                let result = socket.send_to(buf, destination_address);
                let _ = socket.into_raw_fd();

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
