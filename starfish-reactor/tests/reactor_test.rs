use std::{cell::RefCell, rc::Rc, time::Duration};

use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_reactor::cooperative_synchronization::delayed_future::cooperative_sleep;
use starfish_reactor::cooperative_synchronization::event_future::CooperativeEventFuture;
use starfish_reactor::reactor::{Reactor, cooperative_yield};

async fn compute(n: char) -> i32 {
    for i in 0..3 {
        println!("[.] {} {}", n, i);
        cooperative_yield().await;
    }

    println!("[x] {}", n);
    1
}

async fn compute_test() {
    let local_reactor = Reactor::local_instance();

    let result_future_1 = local_reactor.spawn_with_result(compute('A'));
    let result_future_2 = local_reactor.spawn_with_result(compute('B'));

    let result_1 = result_future_1.await;
    let result_2 = result_future_2.await;

    assert_eq!(1, result_1);
    assert_eq!(1, result_2);
}

#[test]
fn test_async_return() {
    let mut reactor = Reactor::new();

    _ = reactor.spawn(compute_test());

    reactor.run();
}

async fn hello_world(event_future: CooperativeEventFuture, num: i32) {
    println!("[!] {}", num);

    if num == 4 {
        event_future.signal();

        cooperative_yield().await;
    } else {
        event_future.cooperative_wait().await;
    }

    println!("[!!] {}", num);

    cooperative_yield().await;

    println!("[!!!] {}", num);
}

#[test]
fn test_task_yield() {
    let mut reactor = Reactor::new();
    let event_future: CooperativeEventFuture = reactor.create_event();

    for i in 1..10 {
        _ = reactor.spawn(hello_world(event_future.clone(), i));
    }

    reactor.run();
}

/*
#[test]
fn verify_oom()
{
    set_alloc_error_hook(custom_alloc_error_hook);

    let panic = catch_unwind(|| {
        // This is guaranteed to exceed even the size of the address space
        for _ in 0..16 {
            // Truncates to a suitable value for both 32-bit and 64-bit targets.
            let alloc_size = 0x1000_0000_1000_0000u64 as usize;
            forget(black_box(vec![0u8; alloc_size]));
        }
    });
    assert!(panic.is_err());
}
*/

#[test]
fn fibonacci_test() {
    async fn calculate_fibonacci(n: u64) -> u64 {
        let local_reactor = Reactor::local_instance();

        match n {
            0 => 1,
            1 => 1,
            n => {
                let result_future_1 = local_reactor.spawn_with_result(calculate_fibonacci(n - 1));
                let result_future_2 = local_reactor.spawn_with_result(calculate_fibonacci(n - 2));

                let n_1 = result_future_1.await;
                let n_2 = result_future_2.await;
                n_1 + n_2
            }
        }
    }

    let mut reactor = Reactor::new();

    let result_future = reactor.spawn_with_result(calculate_fibonacci(15));

    reactor.run();

    assert_eq!(987, result_future.unwrap_result());
}

async fn sleep_sort_item(list: Rc<RefCell<Vec<i32>>>, value: i32, sleep: u64) {
    cooperative_sleep(Duration::from_millis(sleep)).await;

    list.borrow_mut().push(value);
}

#[test]
fn sort_by_sleep_test() {
    let mut reactor = Reactor::new();

    let list = Rc::new(RefCell::new(vec![]));

    for i in (1..10).rev() {
        _ = reactor.spawn(sleep_sort_item(list.clone(), i, i as u64));
    }

    reactor.run();

    assert_eq!((1..10).collect::<Vec<i32>>(), *list.borrow());
}
