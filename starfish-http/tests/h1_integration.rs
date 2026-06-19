use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_http::{HttpClient, HttpServer};
use starfish_reactor::reactor::Reactor;
use std::net::SocketAddr;

fn next_addr() -> SocketAddr {
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    addr
}

#[test]
fn http1_client_and_server_roundtrip_over_single_connection() {
    let mut reactor = Reactor::new();
    let addr = next_addr();

    let server = reactor.spawn_with_result(async move {
        let mut server = HttpServer::bind(addr).await.map_err(|e| e.to_string())?;
        let mut conn = server.accept().await.map_err(|e| e.to_string())?;

        let Some(first_request) = conn.recv_request().await.map_err(|e| e.to_string())? else {
            return Err("missing first request".into());
        };
        if first_request.method() != http::Method::GET || first_request.uri().path() != "/one" {
            return Err(format!(
                "unexpected first request: {} {}",
                first_request.method(),
                first_request.uri()
            ));
        }
        conn.send_response(
            http::Response::builder()
                .status(http::StatusCode::OK)
                .header("x-seq", "one")
                .body(b"first".to_vec())
                .unwrap(),
        )
        .await
        .map_err(|e| e.to_string())?;

        let Some(second_request) = conn.recv_request().await.map_err(|e| e.to_string())? else {
            return Err("missing second request".into());
        };
        if second_request.method() != http::Method::POST
            || second_request.uri().path() != "/two"
            || second_request.body().as_slice() != b"payload"
        {
            return Err(format!(
                "unexpected second request: {} {} {:?}",
                second_request.method(),
                second_request.uri(),
                second_request.body()
            ));
        }
        conn.send_response(
            http::Response::builder()
                .status(http::StatusCode::CREATED)
                .header("x-seq", "two")
                .body(b"second".to_vec())
                .unwrap(),
        )
        .await
        .map_err(|e| e.to_string())?;

        Ok::<bool, String>(true)
    });

    let client = reactor.spawn_with_result(async move {
        let mut client = HttpClient::new();

        let first_request = http::Request::builder()
            .method(http::Method::GET)
            .uri(format!("http://{addr}/one"))
            .header("Host", addr.to_string())
            .body(Vec::new())
            .unwrap();
        let first_response = client
            .send(first_request)
            .await
            .map_err(|e| e.to_string())?;
        if first_response.status() != http::StatusCode::OK
            || first_response.body().as_slice() != b"first"
            || first_response
                .headers()
                .get("x-seq")
                .and_then(|value| value.to_str().ok())
                != Some("one")
        {
            return Err(format!("unexpected first response: {:?}", first_response));
        }

        let second_request = http::Request::builder()
            .method(http::Method::POST)
            .uri(format!("http://{addr}/two"))
            .header("Host", addr.to_string())
            .body(b"payload".to_vec())
            .unwrap();
        let second_response = client
            .send(second_request)
            .await
            .map_err(|e| e.to_string())?;
        if second_response.status() != http::StatusCode::CREATED
            || second_response.body().as_slice() != b"second"
            || second_response
                .headers()
                .get("x-seq")
                .and_then(|value| value.to_str().ok())
                != Some("two")
        {
            return Err(format!("unexpected second response: {:?}", second_response));
        }

        Ok::<bool, String>(true)
    });

    reactor.run();

    assert!(server.unwrap_result().unwrap());
    assert!(client.unwrap_result().unwrap());
}
