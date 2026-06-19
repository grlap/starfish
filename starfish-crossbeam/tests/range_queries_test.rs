use starfish_core::data_structures::SortedCollection;
use starfish_core::data_structures::sorted::skip_list::SkipList;
use starfish_crossbeam::EpochGuard;

#[test]
fn test_range_basic() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    // Insert 0..100
    for i in 0..100 {
        list.insert(i);
    }

    // Standard range (inclusive..exclusive)
    let values: Vec<i32> = list.range(10..20).map(|x| *x).collect();
    assert_eq!(values, (10..20).collect::<Vec<_>>());
}

#[test]
fn test_range_inclusive() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    for i in 0..100 {
        list.insert(i);
    }

    // Inclusive range
    let values: Vec<i32> = list.range(10..=20).map(|x| *x).collect();
    assert_eq!(values, (10..=20).collect::<Vec<_>>());
}

#[test]
fn test_range_from() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    for i in 0..100 {
        list.insert(i);
    }

    // Open-ended from
    let values: Vec<i32> = list.range(90..).map(|x| *x).collect();
    assert_eq!(values, (90..100).collect::<Vec<_>>());
}

#[test]
fn test_range_to() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    for i in 0..100 {
        list.insert(i);
    }

    // Open-ended to
    let values: Vec<i32> = list.range(..10).map(|x| *x).collect();
    assert_eq!(values, (0..10).collect::<Vec<_>>());
}

#[test]
fn test_range_to_inclusive() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    for i in 0..100 {
        list.insert(i);
    }

    // Inclusive to
    let values: Vec<i32> = list.range(..=10).map(|x| *x).collect();
    assert_eq!(values, (0..=10).collect::<Vec<_>>());
}

#[test]
fn test_range_full() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    for i in 0..100 {
        list.insert(i);
    }

    // Full range
    let count = list.range(..).count();
    assert_eq!(count, 100);
}

#[test]
fn test_range_empty() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    for i in 0..100 {
        list.insert(i);
    }

    // Empty range
    let values: Vec<i32> = list.range(50..50).map(|x| *x).collect();
    assert_eq!(values, Vec::<i32>::new());
}

#[test]
fn test_range_zero_copy() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    for i in 0..100 {
        list.insert(i);
    }

    // Zero-copy access
    let sum: i32 = list.range(20..30).map(|x| *x).sum();
    assert_eq!(sum, (20..30).sum());
}

#[test]
fn test_range_nonexistent_bounds() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    // Insert only even numbers
    for i in (0..100).step_by(2) {
        list.insert(i);
    }

    // Range starting at odd number (not in list)
    let values: Vec<i32> = list.range(15..25).map(|x| *x).collect();
    assert_eq!(values, vec![16, 18, 20, 22, 24]);
}

#[test]
fn test_range_beyond_bounds() {
    let list: SkipList<i32, EpochGuard> = SkipList::new();

    for i in 10..20 {
        list.insert(i);
    }

    // Range beyond collection bounds
    let values: Vec<i32> = list.range(0..30).map(|x| *x).collect();
    assert_eq!(values, (10..20).collect::<Vec<_>>());
}
