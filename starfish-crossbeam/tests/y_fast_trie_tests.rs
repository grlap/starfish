use serial_test::serial;
use starfish_core::data_structures::trie::y_fast_trie::YFastTrie;
use starfish_crossbeam::EpochGuard;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

type EpochYFastTrie = YFastTrie<i32, i32, EpochGuard>;

// ============================================================================
// Basic operations
// ============================================================================

#[test]
#[serial]
fn test_basic_crud() {
    let trie = EpochYFastTrie::new();
    assert!(trie.is_empty());

    assert!(trie.insert(10, 100));
    assert!(trie.insert(20, 200));
    assert!(trie.insert(30, 300));
    assert_eq!(trie.len(), 3);

    assert_eq!(trie.get(&10), Some(100));
    assert_eq!(trie.get(&20), Some(200));
    assert_eq!(trie.get(&30), Some(300));
    assert_eq!(trie.get(&99), None);

    assert!(!trie.insert(10, 999)); // duplicate
    assert_eq!(trie.get(&10), Some(100)); // unchanged

    assert_eq!(trie.remove(&20), Some(200));
    assert_eq!(trie.len(), 2);
    assert!(!trie.contains_key(&20));

    assert_eq!(trie.remove(&20), None); // already removed
}

#[test]
#[serial]
fn test_find_and_apply() {
    let trie = EpochYFastTrie::new();
    trie.insert(42, 100);
    let result = trie.find_and_apply(&42, |k, v| *k + *v);
    assert_eq!(result, Some(142));
    assert_eq!(trie.find_and_apply(&99, |_, _| 0), None);
}

// ============================================================================
// Split correctness with epoch reclamation
// ============================================================================

#[test]
#[serial]
fn test_splits_with_epoch_reclamation() {
    // bucket_max = 60 for default universe (2^30). Insert enough to trigger
    // multiple splits, exercising defer_destroy through EpochGuard.
    let trie = EpochYFastTrie::new();

    for i in 0..500 {
        assert!(trie.insert(i, i * 10));
    }
    assert_eq!(trie.len(), 500);

    for i in 0..500 {
        assert_eq!(trie.get(&i), Some(i * 10), "Missing key {}", i);
    }

    // Remove half and verify
    for i in (0..500).step_by(2) {
        assert_eq!(trie.remove(&i), Some(i * 10));
    }
    assert_eq!(trie.len(), 250);

    for i in 0..500 {
        if i % 2 == 0 {
            assert!(!trie.contains_key(&i));
        } else {
            assert!(trie.contains_key(&i), "Missing odd key {}", i);
        }
    }
}

// ============================================================================
// Concurrent tests
// ============================================================================

#[test]
#[serial]
fn test_concurrent_insert_disjoint() {
    let trie = Arc::new(EpochYFastTrie::new());
    let num_threads = 8;
    let per_thread = 1000;

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let trie = Arc::clone(&trie);
            thread::spawn(move || {
                let base = t * per_thread;
                for i in 0..per_thread {
                    let key = base + i;
                    assert!(trie.insert(key, key * 10));
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(trie.len(), (num_threads * per_thread) as usize);

    for i in 0..(num_threads * per_thread) {
        let key = i;
        assert_eq!(trie.get(&key), Some(key * 10), "Missing key {}", key);
    }
}

#[test]
#[serial]
fn test_concurrent_insert_overlapping() {
    // Multiple threads try to insert the same keys — only one should succeed per key.
    let trie = Arc::new(EpochYFastTrie::new());
    let num_threads = 8;
    let key_range = 2000;
    let successful = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let trie = Arc::clone(&trie);
            let successful = Arc::clone(&successful);
            thread::spawn(move || {
                for i in 0..key_range {
                    if trie.insert(i, i) {
                        successful.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Exactly key_range insertions should succeed (one per key).
    assert_eq!(successful.load(Ordering::Relaxed), key_range as usize);
    assert_eq!(trie.len(), key_range as usize);
}

#[test]
#[serial]
fn test_concurrent_mixed_operations() {
    let trie = Arc::new(EpochYFastTrie::new());

    // Pre-populate
    for i in 0..2000 {
        trie.insert(i, i);
    }

    let barrier = Arc::new(Barrier::new(8));

    let handles: Vec<_> = (0..8)
        .map(|t| {
            let trie = Arc::clone(&trie);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                match t % 4 {
                    0 => {
                        // Insert new keys
                        for i in 2000..2500 {
                            let key = i + (t / 4) * 500;
                            trie.insert(key, key);
                        }
                    }
                    1 => {
                        // Read existing keys
                        for i in 0..2000 {
                            let _ = trie.get(&i);
                        }
                    }
                    2 => {
                        // contains_key checks
                        for i in 0..2000 {
                            let _ = trie.contains_key(&i);
                        }
                    }
                    _ => {
                        // Remove some keys
                        for i in (0..500).step_by(2) {
                            let key = i + (t / 4) * 500;
                            trie.remove(&key);
                        }
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Verify consistency
    for i in 0..4000 {
        let key = i;
        let got = trie.get(&key);
        let contains = trie.contains_key(&key);
        if got.is_some() {
            assert!(
                contains,
                "get() returned Some but contains_key() false for {}",
                key
            );
        }
    }
}

#[test]
#[serial]
fn test_concurrent_insert_remove_same_key() {
    // Rapid insert/remove of the same key from multiple threads.
    let trie = Arc::new(EpochYFastTrie::new());
    let inserts = Arc::new(AtomicUsize::new(0));
    let removes = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let trie = Arc::clone(&trie);
            let inserts = Arc::clone(&inserts);
            let removes = Arc::clone(&removes);
            thread::spawn(move || {
                for _ in 0..1000 {
                    if trie.insert(42, 42) {
                        inserts.fetch_add(1, Ordering::Relaxed);
                    }
                    if trie.remove(&42).is_some() {
                        removes.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let ins = inserts.load(Ordering::Relaxed) as i64;
    let rem = removes.load(Ordering::Relaxed) as i64;
    let exists = trie.contains_key(&42);
    let diff = ins - rem;

    assert!(
        diff == 0 || diff == 1,
        "Invariant violated: inserts={}, removes={}, diff={}, exists={}",
        ins,
        rem,
        diff,
        exists
    );

    if diff == 1 {
        assert!(exists, "Key should exist when inserts - removes = 1");
    } else {
        assert!(!exists, "Key should not exist when inserts - removes = 0");
    }
}

// ============================================================================
// CoW-specific concurrent tests
// ============================================================================

#[test]
#[serial]
fn test_cow_concurrent_writers_same_bucket() {
    // All threads insert into the same narrow key range, forcing CAS retries
    // within a single bucket. Verifies no inserts are lost.
    let trie = Arc::new(EpochYFastTrie::new());
    let num_threads = 8;
    let per_thread = 200;
    let barrier = Arc::new(Barrier::new(num_threads));
    let successful = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let trie = Arc::clone(&trie);
            let barrier = Arc::clone(&barrier);
            let successful = Arc::clone(&successful);
            thread::spawn(move || {
                barrier.wait();
                // All threads insert into keys 0..per_thread (same bucket range).
                // Each thread uses a different value to distinguish, but only one
                // insert per key should succeed.
                for i in 0..per_thread {
                    let key = i as i32;
                    let value = (t * per_thread + i) as i32;
                    if trie.insert(key, value) {
                        successful.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    // Exactly per_thread keys should be present (one winner per key).
    assert_eq!(successful.load(Ordering::Relaxed), per_thread);
    assert_eq!(trie.len(), per_thread);

    // Every key must be retrievable.
    for i in 0..per_thread {
        assert!(
            trie.get(&(i as i32)).is_some(),
            "Key {} lost after concurrent CAS contention",
            i
        );
    }
}

#[test]
#[serial]
fn test_cow_readers_see_consistent_snapshots() {
    // Writers continuously insert while readers scan the trie.
    // Readers must never observe a partial or torn Vec state:
    // if get() returns Some(v), then v == key * 10 (not garbage).
    let trie = Arc::new(EpochYFastTrie::new());
    let stop = Arc::new(AtomicBool::new(false));
    let barrier = Arc::new(Barrier::new(6)); // 2 writers + 4 readers

    // Spawn 2 writer threads inserting sequential keys.
    let mut handles: Vec<_> = (0..2)
        .map(|t| {
            let trie = Arc::clone(&trie);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let base = t * 5000;
                for i in 0..5000 {
                    let key = base + i;
                    trie.insert(key, key * 10);
                }
            })
        })
        .collect();

    // Spawn 4 reader threads that continuously read and validate.
    for _ in 0..4 {
        let trie = Arc::clone(&trie);
        let stop = Arc::clone(&stop);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            let mut reads = 0u64;
            while !stop.load(Ordering::Relaxed) {
                // Scan a range of keys, checking value consistency.
                for i in 0..10000 {
                    let key = i;
                    if let Some(val) = trie.get(&key) {
                        assert_eq!(
                            val,
                            key * 10,
                            "Reader saw inconsistent value for key {}: got {}, expected {}",
                            key,
                            val,
                            key * 10
                        );
                        reads += 1;
                    }
                }
                // Limit iterations to avoid running forever if writers finish fast.
                if reads > 50_000 {
                    break;
                }
            }
        }));
    }

    // Wait for writers to finish.
    for h in handles.drain(..2) {
        h.join().unwrap();
    }

    // Signal readers to stop, then join them.
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }

    // Final validation: all 10,000 keys are present with correct values.
    assert_eq!(trie.len(), 10_000);
    for i in 0..10_000 {
        let key = i;
        assert_eq!(
            trie.get(&key),
            Some(key * 10),
            "Missing or incorrect key {} after reader-writer test",
            key
        );
    }
}

#[test]
#[serial]
fn test_cow_cas_retry_insert_remove_contention() {
    // High contention: multiple threads simultaneously insert AND remove from
    // a small key range, maximizing CAS retry frequency within each bucket.
    let trie = Arc::new(EpochYFastTrie::new());
    let key_range = 50; // small range = high per-bucket contention
    let iterations = 2000;
    let num_threads = 8;
    let barrier = Arc::new(Barrier::new(num_threads));
    let total_inserts = Arc::new(AtomicUsize::new(0));
    let total_removes = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let trie = Arc::clone(&trie);
            let barrier = Arc::clone(&barrier);
            let total_inserts = Arc::clone(&total_inserts);
            let total_removes = Arc::clone(&total_removes);
            thread::spawn(move || {
                barrier.wait();
                let mut local_inserts = 0usize;
                let mut local_removes = 0usize;
                for round in 0..iterations {
                    let key = round % key_range;
                    if round % 2 == 0 {
                        if trie.insert(key, key) {
                            local_inserts += 1;
                        }
                    } else if trie.remove(&key).is_some() {
                        local_removes += 1;
                    }
                }
                total_inserts.fetch_add(local_inserts, Ordering::Relaxed);
                total_removes.fetch_add(local_removes, Ordering::Relaxed);
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let ins = total_inserts.load(Ordering::Relaxed);
    let rem = total_removes.load(Ordering::Relaxed);
    let final_len = trie.len();

    // Invariant: final_len == inserts - removes (each successful insert adds 1,
    // each successful remove subtracts 1).
    assert_eq!(
        final_len,
        ins - rem,
        "Count mismatch: len={}, inserts={}, removes={}, expected len={}",
        final_len,
        ins,
        rem,
        ins - rem
    );

    // Every key that exists must be retrievable.
    for i in 0..key_range {
        let key = i;
        let got = trie.get(&key);
        let contains = trie.contains_key(&key);
        match got {
            Some(v) => {
                assert_eq!(v, key, "Wrong value for key {}", key);
                assert!(contains, "get() Some but contains_key() false for {}", key);
            }
            None => {
                assert!(!contains, "get() None but contains_key() true for {}", key);
            }
        }
    }
}

#[test]
#[serial]
fn test_concurrent_split_helping() {
    // Forces many threads to insert into overlapping key ranges that
    // trigger bucket splits simultaneously, exercising the S-note
    // cooperative helping protocol with EpochGuard reclamation.
    let trie = Arc::new(EpochYFastTrie::new());
    let num_threads = 8;
    let per_thread = 500;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|t| {
            let trie = Arc::clone(&trie);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                for i in 0..per_thread {
                    let key = (t * per_thread + i) as i32;
                    trie.insert(key, key * 10);
                }
            })
        })
        .collect();

    for h in handles {
        h.join().unwrap();
    }

    let expected = num_threads * per_thread;
    assert_eq!(trie.len(), expected);

    for t in 0..num_threads {
        for i in 0..per_thread {
            let key = (t * per_thread + i) as i32;
            assert_eq!(
                trie.get(&key),
                Some(key * 10),
                "Key {} lost after concurrent split helping",
                key
            );
        }
    }
}
