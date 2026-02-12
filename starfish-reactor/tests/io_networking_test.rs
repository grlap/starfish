use std::net::{self, ToSocketAddrs};
use std::time::Duration;
use std::{error, io, str, time};

use rstest::rstest;

use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;

use starfish_reactor::cooperative_io::tcp_listener::TcpListener;
use starfish_reactor::cooperative_io::tcp_stream::TcpStream;
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
