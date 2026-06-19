use std::net::{self, ToSocketAddrs};
use std::time::Duration;
use std::{error, io, str, time};

use rstest::rstest;

use starfish_reactor::cooperative_io::async_read::{AsyncRead, AsyncReadExtension};
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

use starfish_reactor::cooperative_io::tcp_listener::TcpListener;
use starfish_reactor::cooperative_io::tcp_stream::TcpStream;
#[cfg(target_os = "linux")]
use starfish_reactor::cooperative_io::udp_socket::UdpEcnCodepoint;
use starfish_reactor::cooperative_io::udp_socket::UdpSocket;

use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_reactor::reactor::Reactor;

async fn http_perform_get_request(url: String) -> Result<String, Box<dyn error::Error>> {
    println!("verify_tcp_connect");

    let socket_address = url.to_socket_addrs().unwrap().next().unwrap();
    let mut tcp_stream = TcpStream::connect(socket_address).await?;

    let request_str =
        "GET / HTTP/1.1\r\nHost: www.wp.pl\r\nUser-Agent: curl\r\nAccept: */*\r\n\r\n";

    let _written_bytes = tcp_stream
        .write(request_str.as_bytes().to_vec().as_mut_slice())
        .await?;

    let mut buffer: Vec<u8> = vec![1; 32 * 1024];

    let read_bytes = tcp_stream.read(buffer.as_mut_slice()).await?;

    let response = str::from_utf8(&buffer[..read_bytes])?.to_string();

    println!("{}", response);

    drop(tcp_stream);

    Ok(response)
}

async fn verify_http_get_request(url: String) -> Result<bool, Box<dyn error::Error>> {
    http_perform_get_request(url).await?;

    Ok(true)
}

#[test]
fn test_tcp_read() {
    let mut reactor = Reactor::new();

    //let mut result_wait = reactor.spawn_with_result(verify_tcp_connect("127.0.0.1:80".to_string()));
    let result_wait =
        reactor.spawn_with_result(verify_http_get_request("www.wp.pl:80".to_string()));

    reactor.run();

    assert!(matches!(result_wait.unwrap_result(), Ok(true)));
}

async fn handle_tcp_client(mut tcp_listener: TcpListener) -> Result<bool, Box<dyn error::Error>> {
    println!("handle_tcp_client");

    let mut tcp_stream = tcp_listener.accept().await?;

    let mut buffer: Vec<u8> = vec![1; 1024];

    let read_bytes = tcp_stream.read(buffer.as_mut_slice()).await?;

    let body = str::from_utf8(&buffer[..read_bytes])?;

    println!("body: {}", body);

    let reply_string = "reply string";

    let written_bytes = tcp_stream
        .write(reply_string.as_bytes().to_vec().as_mut_slice())
        .await?;

    Ok(read_bytes == written_bytes)
}

async fn create_tcp_server(address: String) -> Result<bool, Box<dyn error::Error>> {
    let tcp_listener = TcpListener::bind(address.clone()).await.unwrap();

    let reactor = Reactor::local_instance();
    let result = reactor.spawn_with_result(handle_tcp_client(tcp_listener));

    let client_result = reactor.spawn_with_result(http_perform_get_request(address.clone()));

    result.await?;
    client_result.await?;

    println!("done");

    Ok(true)
}

#[test]
fn test_tcp_server() {
    let mut reactor = Reactor::new();

    let result_wait = reactor.spawn_with_result(create_tcp_server("127.0.0.1:9099".to_string()));

    reactor.run();

    assert!(matches!(result_wait.unwrap_result(), Ok(true)));
}

fn udp_net_server(udp_server_socket: net::UdpSocket) {
    let mut buffer: Vec<u8> = vec![1; 1024];

    let (received, socket_addr) = udp_server_socket.recv_from(&mut buffer).unwrap();

    println!("[]udp_net_server::received {} {:?}", received, socket_addr);

    std::thread::sleep(time::Duration::from_secs(5));

    udp_server_socket
        .send_to(&buffer[..512], socket_addr)
        .unwrap();
}

async fn udp_cooperative_client(
    udp_server_port: u16,
    send_timeout: Option<IOTimeout>,
    recv_timeout: Option<IOTimeout>,
) -> Result<bool, Box<dyn error::Error>> {
    let source_address = "0.0.0.0:0"
        .to_socket_addrs()?
        .next()
        .ok_or(io::Error::other("failed to resolve address"))?;

    let mut udp_client_socket = UdpSocket::bind(source_address).await?;

    let mut buffer: Vec<u8> = vec![1; 1024];

    let destination_address = format!("127.0.0.1:{}", udp_server_port)
        .to_socket_addrs()?
        .next()
        .ok_or(io::Error::other("failed to resolve address"))?;

    let send_bytes = udp_client_socket
        .send_to_with_timeout(destination_address, &mut buffer, &send_timeout)
        .await?;

    println!("[]::udp_cooperative_client::send {}", send_bytes);

    let (recive_bytes, source_address) = udp_client_socket
        .recv_from_with_timeout(&mut buffer, &recv_timeout)
        .await?;

    println!(
        "[]::udp_cooperative_client::recv {} from:{:?}",
        recive_bytes, source_address
    );

    Ok(recive_bytes == 512 && send_bytes == 1024)
}

#[rstest]
#[case(None, None)]
#[case(Some(IOTimeout::from_duration(Duration::from_secs(4))), None)]
fn test_udp_client(
    #[case] send_timeout_in_sec: Option<IOTimeout>,
    #[case] recv_timeout_in_sec: Option<IOTimeout>,
) {
    println!("test_udp_client");

    let udp_server_socket = net::UdpSocket::bind("127.0.0.1:0").unwrap();

    let udp_server_port = udp_server_socket.local_addr().unwrap().port();

    let udp_thread_handle = std::thread::spawn(move || {
        udp_net_server(udp_server_socket);
    });

    let mut reactor = Reactor::new();

    let result_future = reactor.spawn_with_result(udp_cooperative_client(
        udp_server_port,
        send_timeout_in_sec,
        recv_timeout_in_sec,
    ));

    reactor.run();

    assert!(matches!(result_future.unwrap_result(), Ok(true)));

    udp_thread_handle.join().unwrap();
}

// ---- Timeout tests ----

/// TCP accept times out when no client connects.
#[test]
fn test_tcp_accept_timeout() {
    async fn accept_with_short_timeout() -> io::Result<bool> {
        let mut tcp_listener = TcpListener::bind("127.0.0.1:0").await?;
        let timeout = Some(IOTimeout::from_duration(Duration::from_millis(200)));

        let result = tcp_listener.accept_with_timeout(&timeout).await;

        // Should fail — no client will connect within the timeout.
        assert!(result.is_err(), "accept should have timed out");
        Ok(true)
    }

    let mut reactor = Reactor::new();
    let result = reactor.spawn_with_result(accept_with_short_timeout());
    reactor.run();
    assert!(matches!(result.unwrap_result(), Ok(true)));
}

/// TCP read times out when the peer never sends data.
#[test]
fn test_tcp_read_timeout() {
    // Bind a std listener to get an OS-assigned port.
    let std_listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let server_addr = std_listener.local_addr().unwrap();

    // Server thread: accept the connection but never send any data.
    let server_handle = std::thread::spawn(move || {
        let (_stream, _) = std_listener.accept().unwrap();
        // Hold connection open while the client times out.
        std::thread::sleep(Duration::from_secs(3));
    });

    async fn read_with_short_timeout(addr: std::net::SocketAddr) -> io::Result<bool> {
        let mut tcp_stream = TcpStream::connect(addr).await?;
        let timeout = Some(IOTimeout::from_duration(Duration::from_millis(500)));
        let mut buf = vec![0u8; 1024];

        let result = tcp_stream.read_with_timeout(&mut buf, &timeout).await;
        assert!(result.is_err(), "read should have timed out");
        Ok(true)
    }

    let mut reactor = Reactor::new();
    let result = reactor.spawn_with_result(read_with_short_timeout(server_addr));
    reactor.run();
    assert!(matches!(result.unwrap_result(), Ok(true)));

    server_handle.join().unwrap();
}

/// TCP connect times out when targeting a non-routable address.
#[test]
fn test_tcp_connect_timeout() {
    async fn connect_with_short_timeout() -> io::Result<bool> {
        // 192.0.2.1 is TEST-NET-1 (RFC 5737) — packets are dropped, so connect hangs.
        let addr: std::net::SocketAddr = "192.0.2.1:12345".parse().unwrap();
        let timeout = Some(IOTimeout::from_duration(Duration::from_millis(500)));

        let result = TcpStream::connect_with_timeout(addr, &timeout).await;

        assert!(result.is_err(), "connect should have timed out");
        Ok(true)
    }

    let mut reactor = Reactor::new();
    let result = reactor.spawn_with_result(connect_with_short_timeout());
    reactor.run();
    assert!(matches!(result.unwrap_result(), Ok(true)));
}

/// TCP connect fails when the target port has no listener.
#[test]
fn test_tcp_connect_refused() {
    let std_listener = net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = std_listener.local_addr().unwrap();
    drop(std_listener);

    async fn connect_refused(addr: std::net::SocketAddr) -> io::Result<bool> {
        let result = TcpStream::connect(addr).await;
        assert!(
            result.is_err(),
            "connect should fail when no listener exists"
        );
        Ok(true)
    }

    let mut reactor = Reactor::new();
    let result = reactor.spawn_with_result(connect_refused(addr));
    reactor.run();
    assert!(matches!(result.unwrap_result(), Ok(true)));
}

/// UDP recv_from times out when no one sends.
#[test]
fn test_udp_recv_timeout() {
    async fn recv_with_short_timeout() -> io::Result<bool> {
        let source_address: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut udp_socket = UdpSocket::bind(source_address).await?;

        let timeout = Some(IOTimeout::from_duration(Duration::from_millis(200)));
        let mut buf = vec![0u8; 1024];

        let result = udp_socket.recv_from_with_timeout(&mut buf, &timeout).await;

        assert!(result.is_err(), "recv_from should have timed out");
        Ok(true)
    }

    let mut reactor = Reactor::new();
    let result = reactor.spawn_with_result(recv_with_short_timeout());
    reactor.run();
    assert!(matches!(result.unwrap_result(), Ok(true)));
}

// ---- IPv6 UDP tests ----

fn udp6_net_server(udp_server_socket: net::UdpSocket) {
    let mut buffer: Vec<u8> = vec![1; 1024];

    let (received, socket_addr) = udp_server_socket.recv_from(&mut buffer).unwrap();

    println!("[]udp6_net_server::received {} {:?}", received, socket_addr);

    std::thread::sleep(time::Duration::from_secs(5));

    udp_server_socket
        .send_to(&buffer[..512], socket_addr)
        .unwrap();
}

async fn udp6_cooperative_client(
    udp_server_port: u16,
    send_timeout: Option<IOTimeout>,
    recv_timeout: Option<IOTimeout>,
) -> Result<bool, Box<dyn error::Error>> {
    let source_address: std::net::SocketAddr = "[::]:0".parse()?;

    let mut udp_client_socket = UdpSocket::bind(source_address).await?;

    let mut buffer: Vec<u8> = vec![1; 1024];

    let destination_address: std::net::SocketAddr = format!("[::1]:{}", udp_server_port).parse()?;

    let send_bytes = udp_client_socket
        .send_to_with_timeout(destination_address, &mut buffer, &send_timeout)
        .await?;

    println!("[]::udp6_cooperative_client::send {}", send_bytes);

    let (receive_bytes, source_address) = udp_client_socket
        .recv_from_with_timeout(&mut buffer, &recv_timeout)
        .await?;

    println!(
        "[]::udp6_cooperative_client::recv {} from:{:?}",
        receive_bytes, source_address
    );

    assert!(source_address.is_ipv6(), "source address should be IPv6");

    Ok(receive_bytes == 512 && send_bytes == 1024)
}

#[rstest]
#[case(None, None)]
#[case(Some(IOTimeout::from_duration(Duration::from_secs(4))), None)]
fn test_udp6_client(
    #[case] send_timeout_in_sec: Option<IOTimeout>,
    #[case] recv_timeout_in_sec: Option<IOTimeout>,
) {
    println!("test_udp6_client");

    let udp_server_socket = net::UdpSocket::bind("[::1]:0").unwrap();

    let udp_server_port = udp_server_socket.local_addr().unwrap().port();

    let udp_thread_handle = std::thread::spawn(move || {
        udp6_net_server(udp_server_socket);
    });

    let mut reactor = Reactor::new();

    let result_future = reactor.spawn_with_result(udp6_cooperative_client(
        udp_server_port,
        send_timeout_in_sec,
        recv_timeout_in_sec,
    ));

    reactor.run();

    assert!(matches!(result_future.unwrap_result(), Ok(true)));

    udp_thread_handle.join().unwrap();
}

/// IPv6 UDP recv_from times out when no one sends.
#[test]
fn test_udp6_recv_timeout() {
    async fn recv_with_short_timeout() -> io::Result<bool> {
        let source_address: std::net::SocketAddr = "[::1]:0".parse().unwrap();
        let mut udp_socket = UdpSocket::bind(source_address).await?;

        let timeout = Some(IOTimeout::from_duration(Duration::from_millis(200)));
        let mut buf = vec![0u8; 1024];

        let result = udp_socket.recv_from_with_timeout(&mut buf, &timeout).await;

        assert!(result.is_err(), "recv_from should have timed out");
        Ok(true)
    }

    let mut reactor = Reactor::new();
    let result = reactor.spawn_with_result(recv_with_short_timeout());
    reactor.run();
    assert!(matches!(result.unwrap_result(), Ok(true)));
}

#[cfg(target_os = "linux")]
#[test]
fn test_udp_loopback_preserves_ecn_codepoint() {
    let std_server_socket = net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = std_server_socket.local_addr().unwrap();
    let server_socket = UdpSocket::new(std_server_socket);
    let client_socket = UdpSocket::new(net::UdpSocket::bind("127.0.0.1:0").unwrap());

    let mut reactor = Reactor::new();

    let server_result = reactor.spawn_with_result(async move {
        let mut server_socket = server_socket;
        let mut buf = [0u8; 32];
        let (received, _peer_addr, ecn) = server_socket.recv_from_with_ecn(&mut buf).await?;
        assert_eq!(&buf[..received], b"ecn");
        Ok::<Option<UdpEcnCodepoint>, io::Error>(ecn)
    });

    let client_result = reactor.spawn_with_result(async move {
        let mut client_socket = client_socket;
        let mut payload = b"ecn".to_vec();
        let sent = client_socket
            .send_to_with_ecn(server_addr, &mut payload, Some(UdpEcnCodepoint::Ect0))
            .await?;
        Ok::<usize, io::Error>(sent)
    });

    reactor.run();

    assert_eq!(client_result.unwrap_result().unwrap(), 3);
    assert_eq!(
        server_result.unwrap_result().unwrap(),
        Some(UdpEcnCodepoint::Ect0)
    );
}
