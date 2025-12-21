use starfish_core::preemptive_synchronization::future_extension::FutureExtension;
use starfish_reactor::coordinator::Coordinator;

use starfish_epoch::epoch::{Epoch, EpochCounter};

#[test]
fn epoch_init_test() {
    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    let reactor_count = 4;
    coordinator.initialize(reactor_count);

    let result_waits: Vec<_> = coordinator.for_each_reactor_with_result(|| async {
        EpochCounter::local_instance().active_epoch_index()
    });

    coordinator.join_all().unwrap();

    let results: Vec<_> = result_waits
        .into_iter()
        .map(|future| future.unwrap_result())
        .collect();

    assert_eq!(vec![1, 1, 1, 1], results);
}

#[test]
fn epoch_memory_reclamation() {
    let mut coordinator = Coordinator::new();
    coordinator.register_initializer(Epoch::new());

    coordinator.initialize(1);

    let result = coordinator.reactor_instance(0).spawn_external(async {
        {
            let b = Box::new(4);
            let _ = Box::into_raw(b);
            //Epoch::deffered_delete(b);
        }
    });

    coordinator.join_all().unwrap();
}
