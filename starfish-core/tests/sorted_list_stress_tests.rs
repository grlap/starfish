#[cfg(test)]
mod sorted_list_stress_tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    use starfish_core::data_structures::{DeferredCollection, SafeSortedCollection, SortedList};

    // Helper function to create a fresh list for each test
    fn create_test_list() -> Arc<DeferredCollection<usize, SortedList<usize>>> {
        Arc::new(DeferredCollection::new(SortedList::new()))
    }

    #[test]
    fn test_sorted_list_concurrent_insert_remove_same_values() {
        let list = create_test_list();
        let num_threads = 32;
        let values_per_thread = 100;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    for round in 0..10 {
                        for i in 0..values_per_thread {
                            list.insert(i);
                        }

                        for i in 0..values_per_thread {
                            list.delete(&i);
                        }

                        if round % 3 == 0 {
                            let vec = list.to_vec();
                            assert!(vec.len() <= values_per_thread * num_threads);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        thread::sleep(Duration::from_millis(100));
        let final_vec = list.to_vec();
        println!(
            "Final list size after concurrent insert/remove: {}",
            final_vec.len()
        );
    }

    #[test]
    fn test_sorted_list_high_contention_boundaries() {
        let list = create_test_list();
        let num_threads = 24;
        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .enumerate()
            .map(|(t, _)| {
                let list = Arc::clone(&list);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();

                    for i in 0..1000 {
                        match t % 3 {
                            0 => {
                                list.insert(i);
                            }
                            1 => {
                                list.insert(1000000 - i);
                            }
                            2 => {
                                list.delete(&500000);
                            }
                            _ => unreachable!(),
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let vec = list.to_vec();
        for window in vec.windows(2) {
            assert!(window[0] <= window[1], "List is not sorted!");
        }
    }

    #[test]
    fn test_sorted_list_find_during_modifications() {
        let list = create_test_list();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let find_success = Arc::new(AtomicUsize::new(0));
        let find_failure = Arc::new(AtomicUsize::new(0));

        for i in 0..1000 {
            list.insert(i * 2);
        }

        let mut handles = vec![];
        for t in 0..8 {
            let list = Arc::clone(&list);
            let stop = Arc::clone(&stop_flag);
            handles.push(thread::spawn(move || {
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    let val = t * 10000 + i;
                    if i % 2 == 0 {
                        list.insert(val);
                    } else {
                        list.delete(&val);
                    }
                    i += 1;
                }
            }));
        }

        for _ in 0..16 {
            let list = Arc::clone(&list);
            let stop = Arc::clone(&stop_flag);
            let success = Arc::clone(&find_success);
            let failure = Arc::clone(&find_failure);
            handles.push(thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    for i in 0..2000 {
                        if list.contains(&i) {
                            success.fetch_add(1, Ordering::Relaxed);
                        } else {
                            failure.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }

        thread::sleep(Duration::from_secs(3));
        stop_flag.store(true, Ordering::Relaxed);

        for handle in handles {
            handle.join().unwrap();
        }

        println!(
            "Find success: {}, Find failure: {}",
            find_success.load(Ordering::Relaxed),
            find_failure.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn test_sorted_list_delete_and_return_stress_clean() {
        // Create a fresh list with no prior operations
        let list = create_test_list();
        let num_threads = 24;
        let num_values = 10000;

        // Insert initial values into a clean list
        for i in 0..num_values {
            assert!(list.insert(i), "Failed to insert unique value {}", i);
        }

        // Verify no duplicates before deletion
        let initial_vec = list.to_vec();
        assert_eq!(
            initial_vec.len(),
            num_values,
            "Duplicates found before deletion test!"
        );

        let deleted = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let list = Arc::clone(&list);
                let deleted = Arc::clone(&deleted);
                let failed = Arc::clone(&failed);
                thread::spawn(move || {
                    for i in 0..num_values {
                        if list.delete(&i) {
                            deleted.fetch_add(1, Ordering::Relaxed);
                        } else {
                            failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let total_deleted = deleted.load(Ordering::Relaxed);
        let total_failed = failed.load(Ordering::Relaxed);

        println!("Deleted: {}, Failed: {}", total_deleted, total_failed);

        assert_eq!(
            total_deleted, num_values,
            "Each value should be deleted exactly once"
        );

        assert_eq!(
            total_failed,
            (num_threads - 1) * num_values,
            "Each value should fail to delete {} times",
            num_threads - 1
        );

        let final_list = list.to_vec();
        assert_eq!(final_list.len(), 0, "List should be logically empty");
    }

    #[test]
    fn test_sorted_list_alternating_patterns() {
        let list = create_test_list();
        let num_threads = 16;
        let duration = Duration::from_secs(5);
        let stop = Arc::new(AtomicBool::new(false));

        let handles: Vec<_> = (0..num_threads)
            .enumerate()
            .map(|(t, _)| {
                let list = Arc::clone(&list);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    let start = Instant::now();
                    let mut i = 0;

                    while !stop.load(Ordering::Relaxed) {
                        match t % 4 {
                            0 => {
                                for j in 0..100 {
                                    list.insert(i * 100 + j);
                                }
                            }
                            1 => {
                                for _ in 0..100 {
                                    list.delete(&(i * 37 % 10000));
                                }
                            }
                            2 => {
                                if i % 2 == 0 {
                                    for j in 0..50 {
                                        list.insert(j);
                                    }
                                } else {
                                    for j in 0..50 {
                                        list.insert(999999 - j);
                                    }
                                }
                            }
                            3 => {
                                for j in 0..200 {
                                    list.contains(&(j * 13));
                                }
                            }
                            _ => unreachable!(),
                        }

                        i += 1;

                        if start.elapsed() > duration {
                            stop.store(true, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let vec = list.to_vec();
        for window in vec.windows(2) {
            assert!(window[0] <= window[1], "List lost ordering!");
        }

        println!(
            "Alternating patterns test completed, final size: {}",
            vec.len()
        );
    }

    #[test]
    fn test_sorted_list_marked_node_cleanup_isolated() {
        // Create a completely fresh list
        let list = create_test_list();
        let num_threads = 32;
        let values_per_thread = 1000;

        // Phase 1: Insert many values
        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    for i in 0..values_per_thread {
                        list.insert(t * values_per_thread + i);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let size_before = list.to_vec().len();
        println!("Size before deletion: {}", size_before);
        assert_eq!(size_before, num_threads * values_per_thread);

        // Phase 2: Delete everything concurrently
        let delete_success = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let list = Arc::clone(&list);
                let success = Arc::clone(&delete_success);
                thread::spawn(move || {
                    for i in 0..values_per_thread {
                        let key = t * values_per_thread + i;
                        if list.delete(&(key)) {
                            success.fetch_add(1, Ordering::Release);
                        } else {
                            panic!("Failed to delete {} which should exist!", key);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let total_deleted = delete_success.load(Ordering::Relaxed);
        println!("Successfully deleted: {}", total_deleted);
        assert_eq!(
            total_deleted,
            num_threads * values_per_thread,
            "All values should be successfully deleted"
        );

        // Phase 3: Verify logical deletion
        // All deleted nodes should be marked and skipped by to_vec()
        let remaining = list.to_vec();
        println!("Remaining nodes after deletion: {:?}", &remaining);

        assert_eq!(
            remaining.len(),
            0,
            "All nodes should be logically deleted, but found: {:?}",
            remaining
        );

        // Verify operations correctly skip marked nodes
        for i in 0..num_threads * values_per_thread {
            assert!(!list.contains(&i), "Value {} should not be found", i);
            assert_eq!(
                list.contains(&i),
                false,
                "find_value should return None for {}",
                i
            );
        }
    }

    #[test]
    fn test_sorted_list_memory_ordering_verification() {
        let list = create_test_list();
        let data = Arc::new(AtomicUsize::new(0));
        let flag = Arc::new(AtomicBool::new(false));

        let list1 = Arc::clone(&list);
        let data1 = Arc::clone(&data);
        let flag1 = Arc::clone(&flag);

        let producer = thread::spawn(move || {
            data1.store(42, Ordering::Release);
            list1.insert(100);
            flag1.store(true, Ordering::Release);
        });

        let consumer = thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                thread::yield_now();
            }

            assert!(list.contains(&100));
            assert_eq!(data.load(Ordering::Acquire), 42);
        });

        producer.join().unwrap();
        consumer.join().unwrap();
    }

    #[test]
    fn test_sorted_list_extreme_values() {
        let list = create_test_list();
        let num_threads = 16;

        let handles: Vec<_> = (0..num_threads)
            .enumerate()
            .map(|(t, _)| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    for i in 0..1000 {
                        match t % 4 {
                            0 => list.insert(usize::MIN + i),
                            1 => list.insert(usize::MAX - i),
                            2 => list.insert(usize::MAX / 2 + i),
                            3 => list.insert(usize::MAX / 2 - i),
                            _ => unreachable!(),
                        };
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let vec = list.to_vec();
        for window in vec.windows(2) {
            assert!(
                window[0] <= window[1],
                "Ordering failed with extreme values"
            );
        }
    }

    #[test]
    fn test_sorted_list_concurrent_delete_same_value() {
        let list = create_test_list();
        let num_threads = 100;
        let test_value = 42;

        list.insert(test_value);

        let success_count = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let list = Arc::clone(&list);
                let success = Arc::clone(&success_count);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();

                    if list.delete(&test_value) {
                        success.fetch_add(1, Ordering::Relaxed);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(
            success_count.load(Ordering::Relaxed),
            1,
            "Exactly one thread should successfully delete the value"
        );
        assert!(!list.contains(&test_value), "Value should be gone");
    }
}

#[cfg(test)]
mod intense_stress_tests {
    use serial_test::serial;
    use starfish_core::data_structures::{DeferredCollection, SafeSortedCollection, SortedList};

    use std::collections::HashSet;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    // Helper function to create a DeferredCollection-wrapped SortedList
    fn create_test_list() -> Arc<DeferredCollection<usize, SortedList<usize>>> {
        Arc::new(DeferredCollection::new(SortedList::new()))
    }

    #[test]
    fn test_aba_problem_stress() {
        // Test for ABA problems - rapid insert/delete/reinsert of same values
        let list = create_test_list();
        let num_threads = 32;
        let iterations = 100000;
        let key_range = 10; // Small range to force contention

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    for i in 0..iterations {
                        let key = (t + i) % key_range;

                        // Rapid succession of operations on same key
                        list.insert(key);
                        list.delete(&key);
                        list.insert(key);

                        // Occasionally verify no duplicates exist
                        if i % 1000 == 0 {
                            let vec = list.to_vec();
                            let mut seen = HashSet::new();
                            for &item in &vec {
                                assert!(seen.insert(item), "Found duplicate: {}", item);
                            }
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Final verification
        let final_vec = list.to_vec();
        let mut seen = HashSet::new();
        for &item in &final_vec {
            assert!(seen.insert(item), "Found duplicate in final list: {}", item);
        }
    }

    #[test]
    #[serial]
    fn test_linearizability_stress() {
        // Test that operations appear to take effect atomically
        let list = create_test_list();
        let num_threads = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        let num_ops = 10000;

        // Track operation history for linearizability checking
        let history = Arc::new(parking_lot::Mutex::new(Vec::new()));

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let list = Arc::clone(&list);
                let history = Arc::clone(&history);
                thread::spawn(move || {
                    for i in 0..num_ops {
                        let key = t * num_ops + i;
                        let start = Instant::now();

                        // Insert must return true for new key
                        let inserted = list.insert(key);
                        assert!(inserted, "Failed to insert unique key {}", key);

                        // Immediately after insert, must be findable
                        assert!(list.contains(&key), "Key {} not found after insert", key);

                        // Delete must succeed for existing key
                        let deleted = list.delete(&key);
                        assert!(deleted, "Failed to delete existing key {}", key);

                        // After delete, must not be findable
                        assert!(!list.contains(&key), "Key {} found after delete", key);

                        let end = Instant::now();
                        history.lock().push((t, key, start, end));
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        println!(
            "Linearizability test completed with {} operations",
            history.lock().len()
        );
    }

    #[test]
    #[serial]
    fn test_progress_guarantee() {
        // Test lock-freedom: at least one thread makes progress
        let list = create_test_list();

        let num_threads = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);

        let progress_counters: Vec<_> = (0..num_threads)
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect();

        let stop = Arc::new(AtomicBool::new(false));

        let handles: Vec<_> = (0..num_threads)
            .enumerate()
            .map(|(t, _)| {
                let list = Arc::clone(&list);
                let counter = Arc::clone(&progress_counters[t]);
                let stop = Arc::clone(&stop);
                thread::spawn(move || {
                    let mut i = 0;
                    while !stop.load(Ordering::Relaxed) {
                        let key = t * 1_000_000 + i; // Unique keys per thread

                        if list.insert(key) {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }

                        if list.delete(&key) {
                            counter.fetch_add(1, Ordering::Relaxed);
                        }

                        i += 1;
                    }
                })
            })
            .collect();

        // Let threads run for a fixed time
        thread::sleep(Duration::from_secs(5));
        stop.store(true, Ordering::Relaxed);

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify at least one thread made significant progress
        let max_progress = progress_counters
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .max()
            .unwrap();

        assert!(
            max_progress > 1000,
            "No thread made sufficient progress (max: {})",
            max_progress
        );

        // Check that most threads made some progress (lock-freedom)
        let threads_with_progress = progress_counters
            .iter()
            .filter(|c| c.load(Ordering::Relaxed) > 0)
            .count();

        assert!(
            threads_with_progress > num_threads / 2,
            "Too few threads made progress: {}/{}",
            threads_with_progress,
            num_threads
        );
    }

    #[test]
    fn test_memory_stress() {
        // Stress test memory by creating and deleting many nodes
        let list = create_test_list();
        let num_threads = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);

        let num_iterations = 1000;
        let batch_size = 1000;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    for iteration in 0..num_iterations {
                        let base = t * 1000000 + iteration * batch_size;

                        // Insert a batch
                        for i in 0..batch_size {
                            list.insert(base + i);
                        }

                        // Delete the batch
                        for i in 0..batch_size {
                            list.delete(&(base + i));
                        }

                        // Verify list doesn't grow unbounded
                        if iteration % 100 == 0 {
                            let size = list.to_vec().len();
                            assert!(
                                size < batch_size * num_threads,
                                "List growing unbounded: size = {}",
                                size
                            );
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Final check - should be empty or very small
        let final_size = list.to_vec().len();
        println!("Final size after memory stress: {}", final_size);
        assert!(final_size < 100, "Too many nodes remaining: {}", final_size);
    }

    #[test]
    fn test_pathological_insertion_pattern() {
        // Insert in patterns that stress the list structure
        let list = create_test_list();
        let num_threads = 16;

        let handles: Vec<_> = (0..num_threads)
            .enumerate()
            .map(|(t, _)| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    match t % 4 {
                        0 => {
                            // Ascending order
                            for i in 0..10000 {
                                list.insert(i);
                            }
                        }
                        1 => {
                            // Descending order
                            for i in (0..10000).rev() {
                                list.insert(i);
                            }
                        }
                        2 => {
                            // Random middle insertions
                            for i in 0..10000 {
                                list.insert(5000 + (i * 7919) % 5000);
                            }
                        }
                        3 => {
                            // Alternating high-low
                            for i in 0..5000 {
                                list.insert(i);
                                list.insert(9999 - i);
                            }
                        }
                        _ => unreachable!(),
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify correctness
        let vec = list.to_vec();

        // Check sorted order
        for window in vec.windows(2) {
            assert!(window[0] <= window[1], "List not sorted!");
        }

        // Check for duplicates
        let mut seen = HashSet::new();
        for &item in &vec {
            assert!(seen.insert(item), "Found duplicate: {}", item);
        }
    }

    #[test]
    fn test_concurrent_find_and_modify() {
        // Threads finding and modifying overlapping ranges
        let list = create_test_list();
        let num_threads = 32;
        let range_size = 100;

        // Pre-populate
        for i in 0..1000 {
            list.insert(i * 10); // Sparse initial population
        }

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let list = Arc::clone(&list);
                thread::spawn(move || {
                    let start = (t * 50) % 900; // Overlapping ranges

                    for _ in 0..10000 {
                        // Find all in range
                        let mut found = Vec::new();
                        for i in start..start + range_size {
                            if list.contains(&i) {
                                found.push(i);
                            }
                        }

                        // Delete half of found
                        for (idx, &key) in found.iter().enumerate() {
                            if idx % 2 == 0 {
                                list.delete(&key);
                            }
                        }

                        // Insert new ones in gaps
                        for i in start..start + range_size {
                            if i % 7 == t % 7 {
                                list.insert(i);
                            }
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify final state
        let vec = list.to_vec();
        for window in vec.windows(2) {
            assert!(window[0] <= window[1], "List not sorted!");
        }
    }

    #[test]
    fn test_extreme_contention_single_key() {
        // All threads fighting over a single key
        let list = create_test_list();
        let num_threads = 100;
        let ops_per_thread = 1000;
        let the_key = 42;

        let successful_inserts = Arc::new(AtomicUsize::new(0));
        let successful_deletes = Arc::new(AtomicUsize::new(0));

        let barrier = Arc::new(Barrier::new(num_threads));

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let list = Arc::clone(&list);
                let inserts = Arc::clone(&successful_inserts);
                let deletes = Arc::clone(&successful_deletes);
                let barrier = Arc::clone(&barrier);

                thread::spawn(move || {
                    barrier.wait(); // Synchronize start

                    for _ in 0..ops_per_thread {
                        if list.insert(the_key) {
                            inserts.fetch_add(1, Ordering::Relaxed);

                            // Immediately try to delete
                            if list.delete(&the_key) {
                                deletes.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        let total_inserts = successful_inserts.load(Ordering::Relaxed);
        let total_deletes = successful_deletes.load(Ordering::Relaxed);

        println!(
            "Single key contention - Inserts: {}, Deletes: {}",
            total_inserts, total_deletes
        );

        // Inserts and deletes should be balanced (maybe off by 1)
        assert!(
            (total_inserts as i32 - total_deletes as i32).abs() <= 1,
            "Inserts ({}) and deletes ({}) severely unbalanced",
            total_inserts,
            total_deletes
        );

        // Should have processed many operations despite contention
        assert!(
            total_inserts > 100,
            "Too few successful operations under contention"
        );
    }
}
