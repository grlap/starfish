//! Profiling-only binary for sorted collections.
//!
//! Runs a single data structure doing a single operation in a tight loop.
//! Attach an external profiler (samply, Instruments, perf, callgrind).
//!
//! Usage: profile_skiplist <n> <skiplist|skiptrie|crossbeam> <operation> [threads]
//!
//! Operations:
//!   seq-insert, random-insert, seq-find, random-find, find-miss,
//!   mixed-contention, concurrent-insert, concurrent-contention

use crossbeam_skiplist::SkipMap;
use starfish_core::data_structures::SortedCollection;
use starfish_core::data_structures::sorted::skip_list::SkipList;
use starfish_core::data_structures::trie::skip_trie::SkipTrie;
use starfish_crossbeam::EpochGuard;
use std::hint::black_box;
use std::sync::Arc;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type EpochSkipList = SkipList<i64, EpochGuard>;
type EpochSkipTrie = SkipTrie<i64, EpochGuard>;

fn shuffled_keys(n: i64) -> Vec<i64> {
    let mut keys: Vec<i64> = (0..n).collect();
    for i in (1..keys.len()).rev() {
        keys.swap(i, fastrand::usize(..=i));
    }
    keys
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!(
            "Usage: profile_skiplist <n> <skiplist|skiptrie|crossbeam> <operation> [threads]"
        );
        eprintln!("Operations: seq-insert, random-insert, seq-find, random-find, find-miss,");
        eprintln!("            mixed-contention, concurrent-insert, concurrent-contention");
        std::process::exit(1);
    }

    let n: i64 = args[1].parse().expect("n must be an integer");
    let ds = &args[2];
    let op = &args[3];
    let threads: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(8);

    match (ds.as_str(), op.as_str()) {
        // SkipList
        ("skiplist", "seq-insert") => {
            eprintln!("Profiling skiplist seq-insert ({n} elements)...");
            let list = EpochSkipList::default();
            for i in 0..n {
                list.insert(i);
            }
            black_box(&list);
        }
        ("skiplist", "random-insert") => {
            let keys = shuffled_keys(n);
            eprintln!("Profiling skiplist random-insert ({n} elements)...");
            let list = EpochSkipList::default();
            for &k in &keys {
                list.insert(k);
            }
            black_box(&list);
        }
        ("skiplist", "seq-find") => {
            let list = EpochSkipList::default();
            for i in 0..n {
                list.insert(i);
            }
            eprintln!("Profiling skiplist seq-find ({n} elements)...");
            for i in 0..n {
                black_box(list.contains(&i));
            }
        }
        ("skiplist", "random-find") => {
            let list = EpochSkipList::default();
            for i in 0..n {
                list.insert(i);
            }
            let keys = shuffled_keys(n);
            eprintln!("Profiling skiplist random-find ({n} elements)...");
            for &k in &keys {
                black_box(list.contains(&k));
            }
        }
        ("skiplist", "find-miss") => {
            let list = EpochSkipList::default();
            for i in 0..n {
                list.insert(i);
            }
            eprintln!("Profiling skiplist find-miss ({n} elements)...");
            for i in n..2 * n {
                black_box(list.contains(&i));
            }
        }
        ("skiplist", "mixed-contention") => {
            let key_range = 100i64;
            eprintln!("Profiling skiplist mixed-contention ({n} ops, key_range={key_range})...");
            let list = EpochSkipList::default();
            for i in 0..key_range {
                list.insert(i);
            }
            for i in 0..n {
                let key = i % key_range;
                if i % 2 == 0 {
                    list.insert(key);
                } else {
                    list.delete(&key);
                }
            }
            black_box(&list);
        }
        ("skiplist", "concurrent-insert") => {
            let ops = n / threads as i64;
            eprintln!(
                "Profiling skiplist concurrent-insert ({threads} threads, {ops} ops/thread)..."
            );
            let list = Arc::new(EpochSkipList::default());
            std::thread::scope(|s| {
                for t in 0..threads {
                    let list = Arc::clone(&list);
                    s.spawn(move || {
                        let base = (t as i64) * ops;
                        for i in 0..ops {
                            list.insert(base + i);
                        }
                    });
                }
            });
            black_box(&list);
        }
        ("skiplist", "concurrent-contention") => {
            let key_range = 100i64;
            let ops = n / threads as i64;
            eprintln!(
                "Profiling skiplist concurrent-contention ({threads} threads, {ops} ops/thread)..."
            );
            let list = Arc::new(EpochSkipList::default());
            for i in 0..key_range {
                list.insert(i);
            }
            std::thread::scope(|s| {
                for _ in 0..threads {
                    let list = Arc::clone(&list);
                    s.spawn(move || {
                        for i in 0..ops {
                            let key = i % key_range;
                            if i % 2 == 0 {
                                list.insert(key);
                            } else {
                                list.delete(&key);
                            }
                        }
                    });
                }
            });
            black_box(&list);
        }

        // SkipTrie
        ("skiptrie", "seq-insert") => {
            eprintln!("Profiling skiptrie seq-insert ({n} elements)...");
            let trie = EpochSkipTrie::default();
            for i in 0..n {
                trie.insert(i);
            }
            black_box(&trie);
        }
        ("skiptrie", "random-insert") => {
            let keys = shuffled_keys(n);
            eprintln!("Profiling skiptrie random-insert ({n} elements)...");
            let trie = EpochSkipTrie::default();
            for &k in &keys {
                trie.insert(k);
            }
            black_box(&trie);
        }
        ("skiptrie", "seq-find") => {
            let trie = EpochSkipTrie::default();
            for i in 0..n {
                trie.insert(i);
            }
            eprintln!("Profiling skiptrie seq-find ({n} elements)...");
            for i in 0..n {
                black_box(trie.contains(&i));
            }
        }
        ("skiptrie", "random-find") => {
            let trie = EpochSkipTrie::default();
            for i in 0..n {
                trie.insert(i);
            }
            let keys = shuffled_keys(n);
            eprintln!("Profiling skiptrie random-find ({n} elements)...");
            for &k in &keys {
                black_box(trie.contains(&k));
            }
        }
        ("skiptrie", "find-miss") => {
            let trie = EpochSkipTrie::default();
            for i in 0..n {
                trie.insert(i);
            }
            eprintln!("Profiling skiptrie find-miss ({n} elements)...");
            for i in n..2 * n {
                black_box(trie.contains(&i));
            }
        }
        ("skiptrie", "mixed-contention") => {
            let key_range = 100i64;
            eprintln!("Profiling skiptrie mixed-contention ({n} ops, key_range={key_range})...");
            let trie = EpochSkipTrie::default();
            for i in 0..key_range {
                trie.insert(i);
            }
            for i in 0..n {
                let key = i % key_range;
                if i % 2 == 0 {
                    trie.insert(key);
                } else {
                    trie.delete(&key);
                }
            }
            black_box(&trie);
        }
        ("skiptrie", "concurrent-insert") => {
            let ops = n / threads as i64;
            eprintln!(
                "Profiling skiptrie concurrent-insert ({threads} threads, {ops} ops/thread)..."
            );
            let trie = Arc::new(EpochSkipTrie::default());
            std::thread::scope(|s| {
                for t in 0..threads {
                    let trie = Arc::clone(&trie);
                    s.spawn(move || {
                        let base = (t as i64) * ops;
                        for i in 0..ops {
                            trie.insert(base + i);
                        }
                    });
                }
            });
            black_box(&trie);
        }
        ("skiptrie", "concurrent-contention") => {
            let key_range = 100i64;
            let ops = n / threads as i64;
            eprintln!(
                "Profiling skiptrie concurrent-contention ({threads} threads, {ops} ops/thread)..."
            );
            let trie = Arc::new(EpochSkipTrie::default());
            for i in 0..key_range {
                trie.insert(i);
            }
            std::thread::scope(|s| {
                for _ in 0..threads {
                    let trie = Arc::clone(&trie);
                    s.spawn(move || {
                        for i in 0..ops {
                            let key = i % key_range;
                            if i % 2 == 0 {
                                trie.insert(key);
                            } else {
                                trie.delete(&key);
                            }
                        }
                    });
                }
            });
            black_box(&trie);
        }

        // Crossbeam
        ("crossbeam", "seq-insert") => {
            eprintln!("Profiling crossbeam seq-insert ({n} elements)...");
            let map: SkipMap<i64, ()> = SkipMap::new();
            for i in 0..n {
                map.insert(i, ());
            }
            black_box(&map);
        }
        ("crossbeam", "random-insert") => {
            let keys = shuffled_keys(n);
            eprintln!("Profiling crossbeam random-insert ({n} elements)...");
            let map: SkipMap<i64, ()> = SkipMap::new();
            for &k in &keys {
                map.insert(k, ());
            }
            black_box(&map);
        }
        ("crossbeam", "seq-find") => {
            let map: SkipMap<i64, ()> = SkipMap::new();
            for i in 0..n {
                map.insert(i, ());
            }
            eprintln!("Profiling crossbeam seq-find ({n} elements)...");
            for i in 0..n {
                black_box(map.contains_key(&i));
            }
        }
        ("crossbeam", "random-find") => {
            let map: SkipMap<i64, ()> = SkipMap::new();
            for i in 0..n {
                map.insert(i, ());
            }
            let keys = shuffled_keys(n);
            eprintln!("Profiling crossbeam random-find ({n} elements)...");
            for &k in &keys {
                black_box(map.contains_key(&k));
            }
        }
        ("crossbeam", "find-miss") => {
            let map: SkipMap<i64, ()> = SkipMap::new();
            for i in 0..n {
                map.insert(i, ());
            }
            eprintln!("Profiling crossbeam find-miss ({n} elements)...");
            for i in n..2 * n {
                black_box(map.contains_key(&i));
            }
        }
        ("crossbeam", "mixed-contention") => {
            let key_range = 100i64;
            eprintln!("Profiling crossbeam mixed-contention ({n} ops, key_range={key_range})...");
            let map: SkipMap<i64, ()> = SkipMap::new();
            for i in 0..key_range {
                map.insert(i, ());
            }
            for i in 0..n {
                let key = i % key_range;
                if i % 2 == 0 {
                    map.insert(key, ());
                } else {
                    map.remove(&key);
                }
            }
            black_box(&map);
        }
        ("crossbeam", "concurrent-insert") => {
            let ops = n / threads as i64;
            eprintln!(
                "Profiling crossbeam concurrent-insert ({threads} threads, {ops} ops/thread)..."
            );
            let map = Arc::new(SkipMap::new());
            std::thread::scope(|s| {
                for t in 0..threads {
                    let map = Arc::clone(&map);
                    s.spawn(move || {
                        let base = (t as i64) * ops;
                        for i in 0..ops {
                            map.insert(base + i, ());
                        }
                    });
                }
            });
            black_box(&map);
        }
        ("crossbeam", "concurrent-contention") => {
            let key_range = 100i64;
            let ops = n / threads as i64;
            eprintln!(
                "Profiling crossbeam concurrent-contention ({threads} threads, {ops} ops/thread)..."
            );
            let map = Arc::new(SkipMap::new());
            for i in 0..key_range {
                map.insert(i, ());
            }
            std::thread::scope(|s| {
                for _ in 0..threads {
                    let map = Arc::clone(&map);
                    s.spawn(move || {
                        for i in 0..ops {
                            let key = i % key_range;
                            if i % 2 == 0 {
                                map.insert(key, ());
                            } else {
                                map.remove(&key);
                            }
                        }
                    });
                }
            });
            black_box(&map);
        }

        _ => {
            eprintln!("Unknown: {ds} {op}");
            eprintln!("Data structures: skiplist, skiptrie, crossbeam");
            eprintln!("Operations: seq-insert, random-insert, seq-find, random-find, find-miss,");
            eprintln!("            mixed-contention, concurrent-insert, concurrent-contention");
            std::process::exit(1);
        }
    }

    eprintln!("Done.");
}
