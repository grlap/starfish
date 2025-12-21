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

    let mut buffer: Vec<u8> = Vec::with_capacity(buffer_len);
    unsafe {
        buffer.set_len(buffer_len);
    }

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

        let mut buffer: Vec<u8> = vec![1; 1 * 1024 * 1024 * 1024];
        let written = file.write(&mut buffer).await.expect("write failed");

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

    assert_eq!(true, result_future.unwrap_result());
}

#[cfg(target_os = "windows")]
#[test]
fn test_file_read_with_pooling_port_io_manager() {
    use starfish_reactor::cooperative_io::pooling_io_manager::PoolingIOManagerCreateOptions;

    let create_options = PoolingIOManagerCreateOptions::new();
    let mut reactor = Reactor::try_new_with_io_manager(create_options).unwrap();

    let result_future = reactor.spawn_with_result(read_files_async("buffer.temp_2".to_string()));

    reactor.run();

    assert_eq!(true, result_future.unwrap_result());
}
