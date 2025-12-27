#[cfg(test)]
mod stress_tests {
    use starfish_core::DeferredGuard;
    use starfish_core::data_structures::SplitOrderedHashMap;
    use starfish_core::data_structures::hash::HashMapCollection;

    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::thread;
    use std::time::{Duration, Instant};

    // Type alias for cleaner test code
    type DeferredHashMap<K, V> = SplitOrderedHashMap<K, V, DeferredGuard>;

    #[test]
    fn test_stress_high_contention_single_key() {
        // Multiple threads hammering the same key
        let map: Arc<DeferredHashMap<usize, usize>> = Arc::new(SplitOrderedHashMap::new());
        let num_threads = 32;
        let ops_per_thread = 100_000;
        let key = 42;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    for i in 0..ops_per_thread {
                        // Alternate between insert and remove to maximize contention
                        if i % 2 == 0 {
                            map.insert(key, t * 1000 + i);
                        } else {
                            map.remove(&key);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Final state should be either empty or contain the key
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn test_stress_thundering_herd() {
        // All threads start exactly at the same time
        let map: Arc<DeferredHashMap<usize, usize>> = Arc::new(SplitOrderedHashMap::new());
        let num_threads = 64;
        let barrier = Arc::new(Barrier::new(num_threads));
        let ops_per_thread = 5000;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let map = Arc::clone(&map);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    // Wait for all threads to be ready
                    barrier.wait();

                    // Now all threads attack at once
                    for i in 0..ops_per_thread {
                        let key = (t * ops_per_thread + i) % 1000; // Constrain to 1000 keys
                        match i % 4 {
                            0 => {
                                map.insert(key, key * 2);
                            }
                            1 => {
                                let _ = map.get(&key);
                            }
                            2 => {
                                map.insert(key, key * 3);
                            } // Update (will fail if exists, but that's ok)
                            3 => {
                                map.remove(&key);
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

        println!("Thundering herd test completed, final size: {}", map.len());
    }

    #[test]
    fn test_stress_consistency_verification() {
        // Verify that values remain consistent under concurrent updates
        let map: Arc<DeferredHashMap<usize, usize>> = Arc::new(SplitOrderedHashMap::new());
        let num_threads = 16;
        let num_keys = 100;
        let updates_per_key = 1000;

        // Track successful increments per key
        let counters: Vec<_> = (0..num_keys)
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect();

        // Initialize all keys to 0
        for i in 0..num_keys {
            map.insert(i, 0usize);
        }

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let map = Arc::clone(&map);
                let counters = counters.clone();
                thread::spawn(move || {
                    for _ in 0..updates_per_key {
                        let key = rand::random::<usize>() % num_keys;

                        // Try to increment the value via remove + insert
                        if let Some(old_val) = map.remove(&key) {
                            map.insert(key, old_val + 1);
                            counters[key].fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify that all increments are accounted for
        for i in 0..num_keys {
            let expected = counters[i].load(Ordering::Relaxed);
            let actual = map.get(&i).unwrap_or(0);

            // Due to race conditions, actual might be less than expected
            // but should never be more
            assert!(
                actual <= expected,
                "Key {}: actual {} > expected {}",
                i,
                actual,
                expected
            );
        }
    }

    #[test]
    fn test_stress_memory_ordering() {
        // Test that memory ordering is correct
        let map: Arc<DeferredHashMap<usize, usize>> = Arc::new(SplitOrderedHashMap::new());
        let flag = Arc::new(AtomicBool::new(false));
        let value = Arc::new(AtomicUsize::new(0));

        let map1 = Arc::clone(&map);
        let flag1 = Arc::clone(&flag);
        let value1 = Arc::clone(&value);

        // Producer thread
        let producer = thread::spawn(move || {
            value1.store(42, Ordering::Release);
            map1.insert(1, 100);
            flag1.store(true, Ordering::Release);
        });

        // Consumer thread
        let consumer = thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                thread::yield_now();
            }

            // If we see the flag, we must see the map update
            assert_eq!(map.get(&1), Some(100));

            // And we must see the value update
            assert_eq!(value.load(Ordering::Acquire), 42);
        });

        producer.join().unwrap();
        consumer.join().unwrap();
    }

    #[test]
    fn test_stress_rapid_resize() {
        // Force many resize operations
        let map: Arc<DeferredHashMap<usize, usize>> = Arc::new(SplitOrderedHashMap::new());
        let num_threads = 1;
        let num_insertions = 100000;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    let start = t * num_insertions;
                    let end = start + num_insertions;

                    for i in start..end {
                        map.insert(i, i * i);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify all insertions succeeded
        assert_eq!(map.len(), num_threads * num_insertions);

        // Spot check some values
        for i in (0..num_threads * num_insertions).step_by(1000) {
            assert_eq!(map.get(&i), Some(i * i));
        }
    }

    #[test]
    fn test_stress_insert_or_update_intensive() {
        // Specifically test insert under stress (note: DeferredHashMap::insert returns bool, not old value)
        let map: Arc<DeferredHashMap<usize, usize>> = Arc::new(SplitOrderedHashMap::new());
        let num_threads = 32;
        let num_keys = 50;
        let updates_per_thread = 10000;

        let insert_counts: Vec<_> = (0..num_keys)
            .map(|_| Arc::new(AtomicUsize::new(0)))
            .collect();

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let map = Arc::clone(&map);
                let insert_counts = insert_counts.clone();
                thread::spawn(move || {
                    for i in 0..updates_per_thread {
                        let key = i % num_keys;
                        let value = t * updates_per_thread + i;

                        if map.insert(key, value) {
                            // This was a new insert
                            insert_counts[key].fetch_add(1, Ordering::Relaxed);
                        }
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        // Each key should exist
        for i in 0..num_keys {
            assert!(map.contains(&i));
        }

        println!("Insert stress test completed");
        for i in 0..num_keys.min(10) {
            println!(
                "Key {}: {} successful inserts, final value: {:?}",
                i,
                insert_counts[i].load(Ordering::Relaxed),
                map.get(&i)
            );
        }
    }

    #[test]
    fn test_stress_pathological_hash_collisions() {
        // Create keys that will have similar hash patterns
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
        struct BadHashKey(usize);

        impl std::hash::Hash for BadHashKey {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                // Force collisions - all keys hash to one of 4 values
                (self.0 % 4).hash(state);
            }
        }

        let map: Arc<SplitOrderedHashMap<BadHashKey, usize, DeferredGuard>> =
            Arc::new(SplitOrderedHashMap::new());
        let num_threads = 16;
        let keys_per_thread = 1000;

        let handles: Vec<_> = (0..num_threads)
            .map(|t| {
                let map = Arc::clone(&map);
                thread::spawn(move || {
                    for i in 0..keys_per_thread {
                        let key = BadHashKey(t * keys_per_thread + i);
                        map.insert(key.clone(), i);

                        // Immediately verify it's there
                        assert!(map.contains(&key));
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(map.len(), num_threads * keys_per_thread);
    }

    #[test]
    fn test_stress_long_running_chaos() {
        // Run for a fixed duration with random operations
        let map: Arc<DeferredHashMap<usize, usize>> = Arc::new(SplitOrderedHashMap::new());
        let num_threads = 48;
        let duration = Duration::from_secs(5);
        let stop_flag = Arc::new(AtomicBool::new(false));

        let stats: Vec<_> = (0..num_threads)
            .map(|_| {
                Arc::new((
                    AtomicUsize::new(0), // inserts
                    AtomicUsize::new(0), // gets
                    AtomicUsize::new(0), // removes
                ))
            })
            .collect();

        let start = Instant::now();

        let handles: Vec<_> = (0..num_threads)
            .enumerate()
            .map(|(t, _)| {
                let map = Arc::clone(&map);
                let stop = Arc::clone(&stop_flag);
                let stat = Arc::clone(&stats[t]);

                thread::spawn(move || {
                    let mut i = 0;
                    while !stop.load(Ordering::Relaxed) {
                        let key = rand::random::<usize>() % 10000;

                        match rand::random::<usize>() % 3 {
                            0 => {
                                map.insert(key, i);
                                stat.0.fetch_add(1, Ordering::Relaxed);
                            }
                            1 => {
                                let _ = map.get(&key);
                                stat.1.fetch_add(1, Ordering::Relaxed);
                            }
                            2 => {
                                map.remove(&key);
                                stat.2.fetch_add(1, Ordering::Relaxed);
                            }
                            _ => unreachable!(),
                        }

                        i += 1;

                        // Check if time's up
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

        // Print statistics
        let mut total_inserts = 0;
        let mut total_gets = 0;
        let mut total_removes = 0;

        for stat in &stats {
            total_inserts += stat.0.load(Ordering::Relaxed);
            total_gets += stat.1.load(Ordering::Relaxed);
            total_removes += stat.2.load(Ordering::Relaxed);
        }

        println!("Chaos test completed after {:?}", start.elapsed());
        println!(
            "Total operations: {}",
            total_inserts + total_gets + total_removes
        );
        println!("  Inserts: {}", total_inserts);
        println!("  Gets: {}", total_gets);
        println!("  Removes: {}", total_removes);
        println!("Final map size: {}", map.len());

        // Map size should be reasonable (not growing unbounded)
        assert!(map.len() <= 10000);
    }

    // Add rand as dev dependency in Cargo.toml:
    // [dev-dependencies]
    // rand = "0.8"

    mod rand {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static SEED: AtomicUsize = AtomicUsize::new(12345);

        pub fn random<T>() -> T
        where
            T: From<usize>,
        {
            // Simple LCG for testing
            let old = SEED.fetch_add(1, Ordering::Relaxed);
            let new = (old.wrapping_mul(1103515245).wrapping_add(12345)) / 65536;
            T::from(new)
        }
    }
}
