use std::{error, io, net::ToSocketAddrs, str, sync::Arc};

use rcgen::{CertifiedKey, KeyPair};
use rustls::pki_types::{pem::PemObject, PrivateKeyDer};
use rustls::ServerConfig;

use starfish_reactor::cooperative_io::async_read::AsyncRead;
use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWrite;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::tcp_listener::TcpListener;
use starfish_reactor::cooperative_io::tcp_stream::TcpStream;
use starfish_reactor::reactor::Reactor;

use starfish_core::preemptive_synchronization::future_extension::FutureExtension;

use rustls::RootCertStore;
use starfish_tls::accept_any_server_cert_verifier::AcceptAnyServerCertVerifier;
use starfish_tls::tls_client::TlsClient;
use starfish_tls::tls_server::TlsServer;

fn create_client_config() -> rustls::ClientConfig {
    // Load root certificates.
    //
    let root_store = RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.into(),
    };

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    config
        .dangerous()
        .set_certificate_verifier(Arc::new(AcceptAnyServerCertVerifier));

    config
}

fn generate_self_signed_cert() -> Result<CertifiedKey, rcgen::Error> {
    let subject_alt_names = vec!["localhost".to_string()];

    let mut params = rcgen::CertificateParams::new(subject_alt_names)?;
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "localhost");

    let key_pair = KeyPair::generate()?;
    let cert: rcgen::Certificate = params.self_signed(&key_pair)?;

    Ok(CertifiedKey { cert, key_pair })
}

pub async fn tls_http_perform_get_request<S>(
    stream: S,
    server: &str,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>>
where
    S: AsyncRead + AsyncWrite,
{
    let config = create_client_config();

    // Establish TLS connection
    let mut tcp_client = TlsClient::new(stream, config, server).await?;
    // Prepare HTTP request

    let request = format!(
        "GET {} HTTP/1.1\r\n\
         Host: {}\r\n\
         Connection: close\r\n\
         \r\n",
        path, server
    );

    println!("[] HttpRequest: {}", request);

    // Send request.
    //
    tcp_client.write_all(request.as_bytes()).await?;
    tcp_client.flush().await?;

    println!("Request written_all");

    // Read response.
    //
    let mut response_buf = Vec::new();
    loop {
        let mut buf = [0u8; 16384];
        let count = tcp_client.read(&mut buf).await.unwrap();
        if count == 0 {
            break;
        }
        response_buf.extend_from_slice(&buf[..count]);
    }

    // Close connection.
    //
    tcp_client.close().await?;

    // Convert response to string
    let response_str = String::from_utf8_lossy(&response_buf).to_string();

    Ok(response_str)
}

async fn verify_tls_get_request(url: &str) -> Result<bool, Box<dyn error::Error>> {
    let socket_address = url.to_socket_addrs()?.next().ok_or(io::Error::new(
        io::ErrorKind::Other,
        "failed to resolve address",
    ))?;

    println!("socket_address: {} {:?}", url, socket_address);

    let tcp_stream = TcpStream::connect(socket_address).await?;

    let response = tls_http_perform_get_request(tcp_stream, "www.wp.pl", "/").await?;

    println!("[] Received: [{}]", response);

    Ok(true)
}

async fn test_ssl_tcp_server(address: String) -> bool {
    let certified_key = generate_self_signed_cert().unwrap();

    let key_der = PrivateKeyDer::from_pem(
        rustls::pki_types::pem::SectionKind::PrivateKey,
        certified_key.key_pair.serialize_der(),
    )
    .unwrap();

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certified_key.cert.der().to_owned()], key_der)
        .unwrap();

    let mut tcp_listener = TcpListener::bind(address.clone()).await.unwrap();

    let tcp_stream = tcp_listener.accept().await.unwrap();

    let mut tls_server = TlsServer::new(tcp_stream, config).await.unwrap();

    let mut buffer: Vec<u8> = vec![1; 1024];

    let read_bytes = tls_server
        .read(buffer.as_mut_slice())
        .await
        .expect("tcp_read failed");

    let body = str::from_utf8(&buffer[..read_bytes]).unwrap();

    println!("body: {}", body);

    let reply_string = "reply string\n\n";

    let written_bytes = tls_server
        .write(reply_string.as_bytes().to_vec().as_mut_slice())
        .await
        .expect("tcp_write failed");

    println!("written: {}:", written_bytes);

    tls_server.close().await.unwrap();

    true
}

#[test]
fn test_ssl_read() {
    let mut reactor = Reactor::new();

    let result_future = reactor.spawn_with_result(verify_tls_get_request("www.wp.pl:443"));

    reactor.run();

    assert_eq!(true, result_future.unwrap_result().unwrap());
}

#[test]
fn test_ssl_server() {
    let mut reactor = Reactor::new();

    let result_server_wait =
        reactor.spawn_with_result(test_ssl_tcp_server("127.0.0.1:9099".to_string()));

    let result_client_wait = reactor.spawn_with_result(verify_tls_get_request("127.0.0.1:9099"));

    reactor.run();

    assert_eq!(true, result_server_wait.unwrap_result());
    assert_eq!(true, result_client_wait.unwrap_result().unwrap());
}
