//! io_uring lifecycle integration tests: submission backpressure on a tiny
//! ring, and timeout-driven cancellation of in-flight I/O through the reactor.

#![cfg(target_os = "linux")]

use std::fs::{self, OpenOptions};
use std::net::SocketAddr;
use std::time::Duration;

use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::file_open_options::AsyncOpenOptions;
use starfish_reactor::cooperative_io::io_timeout::IOTimeout;
use starfish_reactor::cooperative_io::udp_socket::UdpSocket;
use starfish_reactor::cooperative_io::uring_io_manager::UringIOManagerCreateOptions;
use starfish_reactor::reactor::Reactor;

/// 16 concurrent tasks, each writing then reading 8 chunks through a ring with
/// SQ=2 / CQ=4: submission constantly outruns the ring, exercising the
/// staged-retry (`EBUSY`/ring-full) and CQ-overflow-backlog paths that used to
/// be the #54 use-after-free and the #60 lost-completion accounting hole.
/// Correct data on every file proves no completion was lost or misdelivered.
#[test]
fn tiny_ring_concurrent_file_io_survives_backpressure() {
    const TASKS: usize = 16;
    const CHUNKS: usize = 8;
    const CHUNK_SIZE: usize = 4096;

    let create_options = UringIOManagerCreateOptions::new().with_queue_size(2);
    let mut reactor = Reactor::try_new_with_io_manager(create_options).unwrap();

    let mut results = Vec::new();

    for task_index in 0..TASKS {
        results.push(reactor.spawn_with_result(async move {
            let file_path = format!("buffer.temp_uring_{task_index}");
            _ = fs::remove_file(&file_path);

            {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .as_async_io()
                    .open(&file_path)
                    .expect("failed to create file");

                for chunk_index in 0..CHUNKS {
                    let chunk = vec![(task_index * CHUNKS + chunk_index) as u8; CHUNK_SIZE];
                    file.write_all(&chunk).await.expect("write failed");
                }
            }

            {
                let mut file = OpenOptions::new()
                    .read(true)
                    .write(false)
                    .create(false)
                    .as_async_io()
                    .open(&file_path)
                    .expect("failed to open file");

                for chunk_index in 0..CHUNKS {
                    let mut chunk = vec![0u8; CHUNK_SIZE];
                    file.read_exact(&mut chunk).await.expect("read failed");

                    let expected = (task_index * CHUNKS + chunk_index) as u8;
                    assert!(
                        chunk.iter().all(|&byte| byte == expected),
                        "task {task_index} chunk {chunk_index}: data corrupted"
                    );
                }
            }

            _ = fs::remove_file(&file_path);
            true
        }));
    }

    reactor.run();

    for result in results {
        assert!(result.unwrap_result());
    }
}

/// A UDP receive with no sender must resolve as `TimedOut` via the reactor's
/// timeout Treap -> `AsyncCancel` -> `ECANCELED` completion chain, and the
/// reactor must then exit cleanly (no stuck `active_io`). Covers the
/// previously untested timeout-cancellation path (#29 in the tracker's #73).
#[test]
fn udp_recv_timeout_cancels_inflight_io_and_reactor_exits() {
    let mut reactor = Reactor::new();

    let result = reactor.spawn_with_result(async {
        let address: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let mut socket = UdpSocket::bind(address).await.expect("bind failed");

        let mut buffer = [0u8; 64];
        let timeout = Some(IOTimeout::from_duration(Duration::from_millis(50)));

        let outcome = socket.recv_from_with_timeout(&mut buffer, &timeout).await;

        match outcome {
            Err(error) => error.kind(),
            Ok(_) => panic!("recv_from with no sender must not succeed"),
        }
    });

    reactor.run();

    assert_eq!(result.unwrap_result(), std::io::ErrorKind::TimedOut);
}
