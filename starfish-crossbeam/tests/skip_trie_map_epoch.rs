//! Validation: `SkipTrie<Pair<K,V>>` map (node-replacement UPDATE via
//! `SortedCollectionInternal::update_internal`)
//! under REAL `EpochGuard` reclamation.
//!
//! Unlike the single-level `SortedList` map (whose node-replacement is validated epoch-safe in
//! `sorted_list_map_epoch.rs`), `SkipTrie` is **multi-level**: a truncated skip list plus an
//! x-fast trie index. A node-replacement UPDATE marks the old node, links the same-key new node
//! (forward insertion in the skip list), maintains the x-fast trie (insert new top-level node,
//! remove old), then `defer_destroy`s the old node. Multi-level node-replacement is precisely
//! the shape that produced a use-after-free in the old `SkipList` helping protocol — guaranteeing
//! the old node is unreachable *at every level / via the trie* before retire is the hard part.
//! This test settles whether SkipTrie's implementation actually achieves that.
//!
//! Readers race the replace+free via `find_and_apply` (dereferences the matched node's INLINE
//! value in place under a pinned guard — the access ASan flags on a freed-but-reachable node).
//! `absent` counts any moment a never-removed key reads as missing. Iteration is duplicate-free:
//! `next_node` skips a node's same-key UPDATE replacement, so the iterator checks assert the STRICT
//! invariant (exactly N keys, strictly ascending) concurrently with replace+free. Run under ASan
//! to prove no UAF.

use starfish_core::data_structures::pair::Pair;
use starfish_core::data_structures::{MapCollection, SkipTrie, SortedCollection};
use starfish_crossbeam::EpochGuard;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

type Map = SkipTrie<Pair<i32, u64>, EpochGuard>;

/// Iteration key snapshot — traverses every node under a pinned guard.
fn keys(m: &Map) -> Vec<i32> {
    SortedCollection::iter(m).map(|e| e.get().key).collect()
}

#[test]
fn epoch_skiptrie_map_presence_iter() {
    const KEYS: i32 = 512;
    const SECS: u64 = 6;
    let map = Arc::new(Map::default());
    for k in 0..KEYS {
        assert!(MapCollection::insert(&*map, k, k as u64));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let fail = Arc::new(AtomicBool::new(false));
    let absent = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    // Updaters: multi-level node-replacement UPDATE -> unlink old node (skip list + x-fast
    // trie) -> defer_destroy it.
    for t in 0..8 {
        let (m, stop) = (map.clone(), stop.clone());
        handles.push(std::thread::spawn(move || {
            let mut s = 0x9E3779B97F4A7C15u64 ^ ((t as u64).wrapping_mul(0xD1B54A32D192ED03));
            while !stop.load(Ordering::Relaxed) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let k = (s % KEYS as u64) as i32;
                MapCollection::update(&*m, k, s);
            }
        }));
    }
    // Readers: dereference the matched node's INLINE value in place, racing the replace+free.
    for _ in 0..6 {
        let (m, stop, absent, fail) = (map.clone(), stop.clone(), absent.clone(), fail.clone());
        handles.push(std::thread::spawn(move || {
            let mut s = 0x2545F4914F6CDD1Du64;
            while !stop.load(Ordering::Relaxed) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let k = (s % KEYS as u64) as i32;
                match MapCollection::find_and_apply(&*m, &k, |_, v| *v) {
                    Some(v) => {
                        std::hint::black_box(v);
                    }
                    None => {
                        absent.fetch_add(1, Ordering::Relaxed);
                        fail.store(true, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    // Iterators: full traversal (skip-list level 0) concurrently with replace+free.
    for _ in 0..3 {
        let (m, stop, fail) = (map.clone(), stop.clone(), fail.clone());
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let v = keys(&m);
                // Iteration is duplicate-free: `next_node` skips a node's same-key UPDATE
                // replacement, so a concurrent update never makes a key appear twice.
                if v.len() != KEYS as usize || !v.windows(2).all(|w| w[0] < w[1]) {
                    fail.store(true, Ordering::Relaxed);
                }
            }
        }));
    }

    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(SECS) {
        std::thread::sleep(Duration::from_millis(25));
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    let fv = keys(&map);
    let final_ok = fv.len() == KEYS as usize && fv.windows(2).all(|w| w[0] < w[1]);
    eprintln!(
        "[SKIPTRIE MAP KEYS={KEYS} {SECS}s] absent={} final_ok={}",
        absent.load(Ordering::Relaxed),
        final_ok
    );
    assert!(!fail.load(Ordering::Relaxed) && final_ok);
}

#[test]
fn epoch_skiptrie_map_choke() {
    const N: i32 = 256;
    const HOT: i32 = 128;
    const SECS: u64 = 6;
    let map = Arc::new(Map::default());
    for k in 0..N {
        assert!(MapCollection::insert(&*map, k, k as u64));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let fail = Arc::new(AtomicBool::new(false));
    let absent = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();
    // 12 updaters hammer ONE key: its node is replaced + unlinked (skip list + trie) + freed on
    // essentially every iteration, maximizing the chance a reader is mid-deref of the old node.
    for t in 0..12 {
        let (m, stop) = (map.clone(), stop.clone());
        handles.push(std::thread::spawn(move || {
            let mut s = 0x1234567u64 ^ (t as u64);
            while !stop.load(Ordering::Relaxed) {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                MapCollection::update(&*m, HOT, s);
            }
        }));
    }
    for _ in 0..8 {
        let (m, stop, absent, fail) = (map.clone(), stop.clone(), absent.clone(), fail.clone());
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                match MapCollection::find_and_apply(&*m, &HOT, |_, v| *v) {
                    Some(v) => {
                        std::hint::black_box(v);
                    }
                    None => {
                        absent.fetch_add(1, Ordering::Relaxed);
                        fail.store(true, Ordering::Relaxed);
                    }
                }
            }
        }));
    }
    for _ in 0..3 {
        let (m, stop, fail) = (map.clone(), stop.clone(), fail.clone());
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                // Iteration is duplicate-free even while HOT is replaced: exact count holds.
                if keys(&m).len() != N as usize {
                    fail.store(true, Ordering::Relaxed);
                }
            }
        }));
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(SECS) {
        std::thread::sleep(Duration::from_millis(25));
    }
    stop.store(true, Ordering::Relaxed);
    for h in handles {
        h.join().unwrap();
    }
    eprintln!(
        "[SKIPTRIE MAP CHOKE] absent={} final_len={}",
        absent.load(Ordering::Relaxed),
        keys(&map).len()
    );
    assert!(!fail.load(Ordering::Relaxed));
}
