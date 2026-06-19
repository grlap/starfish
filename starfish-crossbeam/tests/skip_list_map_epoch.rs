//! Validation: production `SkipList<MapEntry<K,V>>` map (value-CAS UPDATE, claim-first
//! tombstone REMOVE) under REAL EpochGuard reclamation. Exercises `insert`/`update`/
//! `remove` (`MapCollection`), the zero-copy guarded read `get_ref` (returns a
//! `GuardedRef<V>` whose deref reads the value under its own pinned guard), and
//! `SortedCollection::iter`. Run under ASan to prove the value-CAS update, the claim
//! path, and the guarded value read are use-after-free-free — the failure class of the
//! removed SkipList *helping* node-replacement UPDATE (the non-helping node-replacement
//! in SortedList/SkipTrie is validated epoch-safe by the sibling tests).
//!
//! ASan invocation (the UAF check only happens under the sanitizer):
//! ```text
//! RUSTFLAGS=-Zsanitizer=address cargo +nightly test -p starfish-crossbeam \
//!   --target x86_64-unknown-linux-gnu --test skip_list_map_epoch \
//!   --test sorted_list_map_epoch --test skip_trie_map_epoch
//! ```

use starfish_core::data_structures::{MapCollection, MapEntry, SkipList, SortedCollection};
use starfish_crossbeam::EpochGuard;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

type Map = SkipList<MapEntry<i32, u64>, EpochGuard>;

/// Iteration key snapshot (values are pub(crate); keys suffice to check structure).
fn keys(m: &Map) -> Vec<i32> {
    SortedCollection::iter(m).map(|e| *e.get().key()).collect()
}

#[test]
fn epoch_map_presence_iter() {
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
    for _ in 0..6 {
        let (m, stop, absent, fail) = (map.clone(), stop.clone(), absent.clone(), fail.clone());
        handles.push(std::thread::spawn(move || {
            let mut s = 0x2545F4914F6CDD1Du64;
            while !stop.load(Ordering::Relaxed) {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                let k = (s % KEYS as u64) as i32;
                // get_ref returns a GuardedRef<u64>; the deref reads the value under the
                // handle's OWN guard. A UAF here — value swapped + freed by a concurrent
                // update while we hold the handle — is exactly what ASan would catch.
                match m.get_ref(&k) {
                    Some(r) => {
                        std::hint::black_box(*r);
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
        "[MAP KEYS={KEYS} {SECS}s] absent={} final_ok={}",
        absent.load(Ordering::Relaxed),
        final_ok
    );
    assert!(!fail.load(Ordering::Relaxed) && final_ok);
}

#[test]
fn epoch_map_choke() {
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
                // Hot key: a value-CAS update lands on HOT on essentially every
                // iteration, so the deref races the swap+defer-free constantly.
                match m.get_ref(&HOT) {
                    Some(r) => {
                        std::hint::black_box(*r);
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
        "[MAP CHOKE] absent={} final_len={}",
        absent.load(Ordering::Relaxed),
        keys(&map).len()
    );
    assert!(!fail.load(Ordering::Relaxed));
}

/// TST-8(a): the previously-untested interleavings — `remove` racing `update`,
/// `get_ref`, AND `insert` of the same keys, under real EpochGuard reclamation.
///
/// The claim-first protocol under test: remove linearizes at the value claim (null
/// tombstone), update's value-CAS fails on the tombstone, insert HELPS finish a
/// tombstoned duplicate's physical delete (mark + unlink) and retries — it never
/// waits on the remover. Invariants asserted:
/// - STABLE keys (never removed): `absent == 0` and updates never fail — a remover
///   churning other keys must not make a stable key transiently vanish.
/// - CHURN keys: at quiescence, `initial + inserts_ok - removes_ok == present` — every
///   successful remove returned exactly one owned value and every successful insert
///   produced exactly one entry (a lost update/insert or double-remove breaks the
///   balance).
/// - Node + value reclamation: remove retires both the claimed value and the node
///   under ASan this is the claim-path UAF check.
#[test]
fn epoch_map_update_remove_insert_churn() {
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

    // Updaters: hammer BOTH stable and churn keys (update racing remove on churn).
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
    // Removers + re-inserters on churn keys: remove claims the value, retires value
    // and node; insert helps finish claim->mark tombstone windows it runs into.
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
    // Readers: stable keys must never read absent; churn keys exercise the
    // tombstone-gated read paths (get_ref + find_and_apply) racing claim+retire.
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
                match m.get_ref(&stable_k) {
                    Some(h) => {
                        std::hint::black_box(*h);
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
        "[MAP CHURN {SECS}s] absent_stable={} inserts_ok={} removes_ok={} present_churn={present_churn} expected={expected} sorted_ok={sorted_ok}",
        absent_stable.load(Ordering::Relaxed),
        inserts_ok.load(Ordering::Relaxed),
        removes_ok.load(Ordering::Relaxed),
    );
    assert!(
        !fail.load(Ordering::Relaxed),
        "stable-key invariant violated"
    );
    assert_eq!(
        present_churn, expected,
        "insert/remove accounting unbalanced"
    );
    assert!(sorted_ok, "iteration order corrupted");
}
