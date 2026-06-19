use std::fs::{self, OpenOptions};

use starfish_reactor::cooperative_io::async_read::AsyncReadExtension;
use starfish_reactor::cooperative_io::async_write::AsyncWriteExtension;
use starfish_reactor::cooperative_io::file_open_options::AsyncOpenOptions;

use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_reactor::reactor::Reactor;

async fn read_file_async(file_path: String) {
    let mut file = OpenOptions::new()
        .read(true)
        .write(false)
        .create(false)
        .as_async_io()
        .open(file_path)
        .expect("failed to create a file");

    let buffer_len: usize = 10 * 1024 * 1024;

    let mut buffer: Vec<u8> = vec![0; buffer_len];

    println!("[] => load");
    let read_count = file.read(&mut buffer).await.expect("read failed");

    println!("[] => finish");
    assert_eq!(read_count, buffer_len);
}

async fn read_files_async(file_path: String) -> bool {
    // Prepare a test file.
    //
    _ = fs::remove_file(file_path.clone());

    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .as_async_io()
            .open(file_path.clone())
            .expect("failed to create a file");

        let buffer: Vec<u8> = vec![1; 1024 * 1024 * 1024];
        let written = file.write(&buffer).await.expect("write failed");

        assert_eq!(written, buffer.len());
    }

    let reactor = Reactor::local_instance();
    let result_wait_1 = reactor.spawn(read_file_async(file_path.clone()));
    let result_wait_2 = reactor.spawn(read_file_async(file_path.clone()));

    result_wait_1.await;
    result_wait_2.await;

    println!("[] => .");

    // Remove the file
    //
    let res = fs::remove_file(file_path);
    println!("[] delete file => .{:?}", res);

    true
}

#[test]
fn test_file_read() {
    let mut reactor = Reactor::new();

    let result_future = reactor.spawn_with_result(read_files_async("buffer.temp_1".to_string()));

    reactor.run();

    assert!(result_future.unwrap_result());
}

async fn sequential_write_then_read_async(file_path: String) -> bool {
    _ = fs::remove_file(&file_path);

    let chunk_a: Vec<u8> = vec![0xAA; 8192];
    let chunk_b: Vec<u8> = vec![0xBB; 8192];

    // Two separate write() calls on the same File must land sequentially:
    // the second write must continue at the file position, not restart at
    // byte 0 and overwrite the first chunk.
    //
    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .as_async_io()
            .open(file_path.clone())
            .expect("failed to create a file");

        file.write_all(&chunk_a).await.expect("write A failed");
        file.write_all(&chunk_b).await.expect("write B failed");
    }

    // Verify through std I/O that both chunks are present, concatenated.
    //
    let contents = fs::read(&file_path).expect("std read failed");
    assert_eq!(
        contents.len(),
        chunk_a.len() + chunk_b.len(),
        "second write must append at the file position, not overwrite offset 0"
    );
    assert_eq!(&contents[..chunk_a.len()], &chunk_a[..]);
    assert_eq!(&contents[chunk_a.len()..], &chunk_b[..]);

    // Two sequential reads through the async path must advance the cursor.
    //
    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(false)
            .create(false)
            .as_async_io()
            .open(file_path.clone())
            .expect("failed to open the file");

        let mut read_a = vec![0u8; 8192];
        let mut read_b = vec![0u8; 8192];
        file.read_exact(&mut read_a).await.expect("read A failed");
        file.read_exact(&mut read_b).await.expect("read B failed");

        assert_eq!(read_a, chunk_a, "first read must return the first chunk");
        assert_eq!(
            read_b, chunk_b,
            "second read must advance the cursor, not re-read offset 0"
        );
    }

    _ = fs::remove_file(&file_path);

    true
}

/// Regression test for the offset-0 file I/O bug (Linux #51 / Windows #49):
/// repeated reads/writes on the same `File` must follow the file position.
#[test]
fn test_sequential_writes_and_reads_advance_file_position() {
    let mut reactor = Reactor::new();

    let result_future = reactor.spawn_with_result(sequential_write_then_read_async(
        "buffer.temp_seq".to_string(),
    ));

    reactor.run();

    assert!(result_future.unwrap_result());
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
async fn read_to_eof_async(file_path: String) -> bool {
    _ = fs::remove_file(&file_path);

    let payload: Vec<u8> = vec![0xCC; 4096];

    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .as_async_io()
            .open(file_path.clone())
            .expect("failed to create a file");

        file.write_all(&payload).await.expect("write failed");
    }

    {
        let mut file = OpenOptions::new()
            .read(true)
            .write(false)
            .create(false)
            .as_async_io()
            .open(file_path.clone())
            .expect("failed to open the file");

        let mut buffer = vec![0u8; payload.len()];
        file.read_exact(&mut buffer).await.expect("read failed");
        assert_eq!(buffer, payload);

        let eof_read = file
            .read(&mut buffer)
            .await
            .expect("read at EOF must not error");
        assert_eq!(eof_read, 0, "read at EOF must return 0 bytes");
    }

    _ = fs::remove_file(&file_path);

    true
}

/// Reading at EOF must return `Ok(0)` — POSIX `read(2)` semantics, which the
/// Linux backend inherits from the kernel file position and Windows must map
/// from `ERROR_HANDLE_EOF` (an overlapped read at/past EOF *fails* instead of
/// returning 0 bytes). Only reachable in normal sequential reading since file
/// positions advance (the #49/#51 fixes). Excluded on macOS until the EOF
/// continuation-loop bug (#48) is fixed.
#[cfg(any(target_os = "linux", target_os = "windows"))]
#[test]
fn test_read_at_eof_returns_zero() {
    let mut reactor = Reactor::new();

    let result_future = reactor.spawn_with_result(read_to_eof_async("buffer.temp_eof".to_string()));

    reactor.run();

    assert!(result_future.unwrap_result());
}

#[cfg(target_os = "windows")]
#[test]
fn test_file_read_with_pooling_port_io_manager() {
    use starfish_reactor::cooperative_io::pooling_io_manager::PoolingIOManagerCreateOptions;

    let create_options = PoolingIOManagerCreateOptions::new();
    let mut reactor = Reactor::try_new_with_io_manager(create_options).unwrap();

    let result_future = reactor.spawn_with_result(read_files_async("buffer.temp_2".to_string()));

    reactor.run();

    assert!(result_future.unwrap_result());
}
