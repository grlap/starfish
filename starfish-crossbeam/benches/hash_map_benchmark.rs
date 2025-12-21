//! Benchmark for SplitOrderedHashMap with epoch-based memory reclamation.
//!
//! Run with: cargo bench --package starfish-crossbeam --bench hash_map_benchmark

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use mimalloc::MiMalloc;
use std::sync::Arc;
use std::thread;

use starfish_core::data_structures::SplitOrderedHashMap;
use starfish_core::data_structures::hash::SafeHashMapCollection;
use starfish_crossbeam::EpochGuardedHashMap;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// ============================================================================
// Concurrent insert benchmark
// ============================================================================

fn split_ordered_hash_table_insert(thread_count: usize, iteration_count: usize) {
    let table = Arc::new(EpochGuardedHashMap::new(SplitOrderedHashMap::new()));
    let mut handles = vec![];

    for i in 0..thread_count {
        let table_clone = Arc::clone(&table);
        let handle = thread::spawn(move || {
            for j in 0..iteration_count {
                table_clone.insert(
                    i * iteration_count + j,
                    format!("value_{}", i * iteration_count + j),
                );
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(table.len(), iteration_count * thread_count);

    // Verify some random samples
    assert_eq!(
        table.get(&42).map(|v| v.clone()),
        Some("value_42".to_string())
    );
    assert_eq!(
        table.get(&500).map(|v| v.clone()),
        Some("value_500".to_string())
    );
    assert_eq!(
        table.get(&999).map(|v| v.clone()),
        Some("value_999".to_string())
    );
}

// ============================================================================
// Mixed operations benchmark (insert + get + remove)
// ============================================================================

fn split_ordered_hash_table_mixed(thread_count: usize, iteration_count: usize) {
    let table = Arc::new(EpochGuardedHashMap::new(SplitOrderedHashMap::new()));

    // Pre-populate with half the values
    for i in 0..(thread_count * iteration_count / 2) {
        table.insert(i, format!("value_{}", i));
    }

    let mut handles = vec![];

    for t in 0..thread_count {
        let table_clone = Arc::clone(&table);
        let handle = thread::spawn(move || {
            let base = t * iteration_count;
            for i in 0..iteration_count {
                match i % 3 {
                    0 => {
                        // Insert new value
                        table_clone.insert(base + i + 1_000_000, format!("new_{}", base + i));
                    }
                    1 => {
                        // Get existing value
                        let _ = table_clone.get(&(i / 2));
                    }
                    2 => {
                        // Remove existing value
                        table_clone.remove(&(i / 2));
                    }
                    _ => unreachable!(),
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

// ============================================================================
// High contention benchmark
// ============================================================================

fn split_ordered_hash_table_contention(thread_count: usize, iteration_count: usize) {
    let table = Arc::new(EpochGuardedHashMap::new(SplitOrderedHashMap::new()));
    let key_range = 100usize;

    let mut handles = vec![];

    for _ in 0..thread_count {
        let table_clone = Arc::clone(&table);
        let handle = thread::spawn(move || {
            for i in 0..iteration_count {
                let key = i % key_range;
                if i % 2 == 0 {
                    table_clone.insert(key, format!("value_{}", i));
                } else {
                    table_clone.remove(&key);
                }
            }
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }
}

// ============================================================================
// Criterion benchmark groups
// ============================================================================

fn concurrent_insert_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_concurrent_insert");

    for thread_count in [1, 2, 4, 8, 12, 16] {
        let bench_name = format!("split_ordered_hash_map_{:0>2}_10000", thread_count);
        group.bench_function(bench_name, |b| {
            b.iter(|| split_ordered_hash_table_insert(black_box(thread_count), black_box(10_000)))
        });
    }

    group.finish();
}

fn mixed_operations_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_mixed_operations");

    for thread_count in [1, 2, 4, 8, 12, 16] {
        let bench_name = format!("split_ordered_hash_map_{:0>2}_10000", thread_count);
        group.bench_function(bench_name, |b| {
            b.iter(|| split_ordered_hash_table_mixed(black_box(thread_count), black_box(10_000)))
        });
    }

    group.finish();
}

fn contention_benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("hash_map_high_contention");

    for thread_count in [1, 2, 4, 8, 12, 16] {
        let bench_name = format!("split_ordered_hash_map_{:0>2}_10000", thread_count);
        group.bench_function(bench_name, |b| {
            b.iter(|| {
                split_ordered_hash_table_contention(black_box(thread_count), black_box(10_000))
            })
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    concurrent_insert_benchmark,
    mixed_operations_benchmark,
    contention_benchmark
);
criterion_main!(benches);
