//! Validation: `SortedList<Pair<K,V>>` map (node-replacement UPDATE via
//! `SortedCollectionInternal::update_internal`) under REAL `EpochGuard` reclamation.
//!
//! Unlike the `SkipList<MapEntry>` map (value-CAS), a `SortedList` map UPDATE replaces the
//! whole node: insert a new same-key node after the old one (forward insertion, marking the
//! old node UPDATE), fully unlink the old node, then retire it INTERNALLY. The question this
//! test answers is whether that is epoch-safe — `pair.rs` historically flagged inline-`V`
//! node-replacement as suspect, and the analogous *multi-level + helping* SkipList variant
//! produced a use-after-free. SortedList is single-level, has no helping, and
//! `unlink_marked_node` provably unlinks the old node before it is returned for deferral, so
//! the old node is unreachable-from-head before retire and a reader sitting on it follows the
//! UPDATE mark forward to the live new node.
//!
//! Readers race the replace+free via `find_and_apply`, which dereferences the node's INLINE
//! value in place under its pinned guard — exactly the access ASan would flag if the old node
//! were freed while still reachable. `absent` counts any moment a never-removed key reads as
//! missing (the key-always-present invariant of forward-insertion UPDATE). Run under ASan to
//! prove no use-after-free.
//!
//! Iteration is duplicate-free: `next_node_internal` skips a node's same-key UPDATE replacement
//! (forward insertion links the replacement immediately after the old node), so a concurrent
//! UPDATE never makes a key appear twice. The iterator checks below therefore assert the STRICT
//! invariant — exactly KEYS keys, strictly ascending — concurrently with replace+free. Point
//! reads (`find_and_apply`) also skip marked nodes, so `absent` stays 0.

use starfish_core::data_structures::pair::Pair;
use starfish_core::data_structures::{MapCollection, SortedCollection, SortedList};
use starfish_crossbeam::EpochGuard;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

type Map = SortedList<Pair<i32, u64>, EpochGuard>;

/// Iteration key snapshot — traverses every node under a pinned guard.
fn keys(m: &Map) -> Vec<i32> {
    SortedCollection::iter(m).map(|e| e.get().key).collect()
}

#[test]
fn epoch_sorted_map_presence_iter() {
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

    // Updaters: node-replacement UPDATE -> unlink old node -> defer_destroy it.
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
    // Readers: find_and_apply dereferences the matched node's INLINE value in place under a
    // pinned guard, racing a concurrent updater that unlinks+frees that very node.
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
    // Iterators: full traversal of every node concurrently with replace+free.
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
        "[SORTED MAP KEYS={KEYS} {SECS}s] absent={} final_ok={}",
        absent.load(Ordering::Relaxed),
        final_ok
    );
    assert!(!fail.load(Ordering::Relaxed) && final_ok);
}

#[test]
fn epoch_sorted_map_choke() {
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
    // 12 updaters hammer ONE key: its node is replaced + unlinked + freed on essentially
    // every iteration, maximizing the chance a reader is mid-deref of the old node.
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
                // Read the hot key's inline value in place while it is being replaced+freed.
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
        "[SORTED MAP CHOKE] absent={} final_len={}",
        absent.load(Ordering::Relaxed),
        keys(&map).len()
    );
    assert!(!fail.load(Ordering::Relaxed));
}
