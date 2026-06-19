//! Validation: `SkipList<Pair<K,V>>` map (node-replacement UPDATE via
//! `SortedCollectionInternal::update_internal` — the forward-splice protocol)
//! under REAL `EpochGuard`
//! reclamation.
//!
//! This is the research-track backend: the value lives INLINE in the node (no
//! `AtomicPtr` indirection), `update` splices a same-key replacement after the
//! old node with one CAS (`old.next[0]: succ -> new|UPDATE_MARK` — the key is
//! NEVER absent), and the old node's teardown reuses the deferred physical
//! delete machinery (`linked_height` handshake picks exactly one runner;
//! nothing ever waits on another thread). The historical SkipList
//! node-replacement UPDATE was removed precisely because its teardown could
//! retire a still-reachable node (heap-use-after-free under this very test
//! shape); the questions this file answers under ASan are:
//!
//! - is the replaced node always unreachable before retirement (no UAF), even
//!   with multi-level towers, deferred teardowns, and same-key zombie runs at
//!   routing levels?
//! - does a never-removed key ever read absent (`absent` must stay 0 — the
//!   key-always-present invariant of the splice)?
//! - is iteration duplicate-free while keys are being replaced (the same-key
//!   replacement skip), and does update↔remove↔insert churn balance exactly?
//!
//! Readers race replace+retire via `find_and_apply`, dereferencing the INLINE
//! value in place under a pinned guard — exactly the access ASan flags if the
//! old node is freed while reachable. The delete-loses-to-UPDATE retry
//! is exercised continuously by the churn test's remove/update races.

use starfish_core::data_structures::pair::Pair;
use starfish_core::data_structures::sorted::skip_list::SkipList;
use starfish_core::data_structures::{MapCollection, SortedCollection};
use starfish_crossbeam::EpochGuard;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

type Map = SkipList<Pair<i32, u64>, EpochGuard>;

/// Iteration key snapshot — traverses every node under a pinned guard.
fn keys(m: &Map) -> Vec<i32> {
    SortedCollection::iter(m).map(|e| e.get().key).collect()
}

#[test]
fn epoch_skiplist_pair_presence_iter() {
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

    // Updaters: forward-splice UPDATE -> handshake teardown -> retire old node.
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
    // Readers: dereference the matched node's INLINE value in place under a
    // pinned guard, racing the updater that unlinks+retires that very node.
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
    // Iterators: full traversal concurrently with replace+retire. STRICT
    // invariant: exactly KEYS keys, strictly ascending (the same-key
    // replacement skip means a concurrent update never yields a key twice).
    for _ in 0..3 {
        let (m, stop, fail) = (map.clone(), stop.clone(), fail.clone());
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let v = keys(&m);
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
        "[SKIPLIST PAIR KEYS={KEYS} {SECS}s] absent={} final_ok={}",
        absent.load(Ordering::Relaxed),
        final_ok
    );
    assert!(!fail.load(Ordering::Relaxed) && final_ok);
}

#[test]
fn epoch_skiplist_pair_choke() {
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
    // 12 updaters hammer ONE key: its node is replaced + unlinked + retired on
    // essentially every iteration (update-vs-update splice races resolve by
    // following the winner's replacement), maximizing both the reader-mid-deref
    // window and replacement-chain depth. The historical helping UPDATE
    // livelocked/UAF'd under exactly this shape.
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
        "[SKIPLIST PAIR CHOKE] absent={} final_len={}",
        absent.load(Ordering::Relaxed),
        keys(&map).len()
    );
    assert!(!fail.load(Ordering::Relaxed));
    assert_eq!(keys(&map).len(), N as usize);
}

/// The full interaction matrix under real reclamation: `update` (splice) racing
/// `remove` (DELETE mark + lost-to-update retry) racing `insert` (same keys) — plus the
/// deferred-teardown handoffs and equal-key zombie runs those races create.
///
/// Invariants:
/// - STABLE keys (never removed): `absent == 0` AND their updates never fail —
///   a delete losing to an update must retry onto the replacement, never
///   report the key absent, and churn on other keys must not make a
///   stable key vanish.
/// - CHURN keys at quiescence: `initial + inserts_ok - removes_ok == present`
///   (a lost insert/update, double-remove, or remove-of-replaced-node
///   accounting error breaks the balance).
/// - Iteration stays strictly sorted (zombie runs never surface).
#[test]
fn epoch_skiplist_pair_update_remove_insert_churn() {
    const STABLE: i32 = 256;
    const CHURN: i32 = 64; // keys STABLE..STABLE+CHURN
    const SECS: u64 = 6;
    let map = Arc::new(Map::default());
    for k in 0..(STABLE + CHURN) {
        assert!(MapCollection::insert(&*map, k, k as u64));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let fail = Arc::new(AtomicBool::new(false));
    let absent_stable = Arc::new(AtomicU64::new(0));
    let inserts_ok = Arc::new(AtomicU64::new(0));
    let removes_ok = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    // Updaters: hammer BOTH stable and churn keys (update racing remove).
    for t in 0..6 {
        let (m, stop, fail) = (map.clone(), stop.clone(), fail.clone());
        handles.push(std::thread::spawn(move || {
            let mut s = 0x9E3779B97F4A7C15u64 ^ ((t as u64).wrapping_mul(0xD1B54A32D192ED03));
            while !stop.load(Ordering::Relaxed) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let k = (s % (STABLE + CHURN) as u64) as i32;
                let ok = MapCollection::update(&*m, k, s);
                // A stable key is never removed: its update must never fail.
                if k < STABLE && !ok {
                    fail.store(true, Ordering::Relaxed);
                }
            }
        }));
    }
    // Removers + re-inserters on churn keys: remove marks the carrier (retrying
    // past lost-to-update splices); insert snips/resolves zombie blockers.
    for t in 0..6 {
        let (m, stop, ins, rem) = (
            map.clone(),
            stop.clone(),
            inserts_ok.clone(),
            removes_ok.clone(),
        );
        handles.push(std::thread::spawn(move || {
            let mut s = 0xDEADBEEFu64 ^ ((t as u64) << 32);
            while !stop.load(Ordering::Relaxed) {
                s = s.wrapping_mul(6364136223846793005).wrapping_add(1);
                let k = STABLE + (s % CHURN as u64) as i32;
                if s & 1 == 0 {
                    if MapCollection::remove(&*m, &k).is_some() {
                        rem.fetch_add(1, Ordering::Relaxed);
                    }
                } else if MapCollection::insert(&*m, k, s) {
                    ins.fetch_add(1, Ordering::Relaxed);
                }
            }
        }));
    }
    // Readers: stable keys must never read absent; churn keys exercise reads
    // racing splice/mark/teardown; iterators check sortedness throughout.
    for r in 0..6 {
        let (m, stop, absent, fail) = (
            map.clone(),
            stop.clone(),
            absent_stable.clone(),
            fail.clone(),
        );
        handles.push(std::thread::spawn(move || {
            let mut s = 0x2545F4914F6CDD1Du64 ^ (r as u64);
            while !stop.load(Ordering::Relaxed) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let stable_k = (s % STABLE as u64) as i32;
                match MapCollection::find_and_apply(&*m, &stable_k, |_, v| *v) {
                    Some(v) => {
                        std::hint::black_box(v);
                    }
                    None => {
                        absent.fetch_add(1, Ordering::Relaxed);
                        fail.store(true, Ordering::Relaxed);
                    }
                }
                let churn_k = STABLE + (s % CHURN as u64) as i32;
                std::hint::black_box(MapCollection::find_and_apply(&*m, &churn_k, |_, v| *v));
            }
        }));
    }
    for _ in 0..2 {
        let (m, stop, fail) = (map.clone(), stop.clone(), fail.clone());
        handles.push(std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let v = keys(&m);
                if !v.windows(2).all(|w| w[0] < w[1]) {
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

    // Quiescent accounting on churn keys: initial + inserts_ok - removes_ok == present.
    let present_churn = (STABLE..STABLE + CHURN)
        .filter(|k| MapCollection::contains(&*map, k))
        .count() as i64;
    let expected = CHURN as i64 + inserts_ok.load(Ordering::Relaxed) as i64
        - removes_ok.load(Ordering::Relaxed) as i64;
    let fv = keys(&map);
    let sorted_ok = fv.windows(2).all(|w| w[0] < w[1]);
    eprintln!(
        "[SKIPLIST PAIR CHURN {SECS}s] absent_stable={} inserts_ok={} removes_ok={} present_churn={present_churn} expected={expected} sorted_ok={sorted_ok}",
        absent_stable.load(Ordering::Relaxed),
        inserts_ok.load(Ordering::Relaxed),
        removes_ok.load(Ordering::Relaxed),
    );
    assert!(
        !fail.load(Ordering::Relaxed),
        "stable-key / sortedness invariant violated"
    );
    assert_eq!(
        present_churn, expected,
        "insert/remove accounting unbalanced"
    );
    assert!(sorted_ok, "iteration order corrupted");
}
