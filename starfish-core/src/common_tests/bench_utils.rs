//! Shared benchmark utilities.
//!
//! Provides helpers used across multiple benchmark binaries, such as
//! dynamic thread-count generation based on available parallelism.

/// Returns thread counts for a given maximum: powers of 2 up to `max`,
/// plus `max` itself if it isn't a power of 2.
///
/// # Examples
///
/// ```
/// # use starfish_core::common_tests::bench_utils::thread_counts_for;
/// assert_eq!(thread_counts_for(8), vec![1, 2, 4, 8]);
/// assert_eq!(thread_counts_for(24), vec![1, 2, 4, 8, 16, 24]);
/// ```
pub fn thread_counts_for(max: usize) -> Vec<usize> {
    let max = max.max(1);
    let mut counts: Vec<usize> = (0..usize::BITS)
        .map(|i| 1usize << i)
        .take_while(|&n| n <= max)
        .collect();
    if counts.last() != Some(&max) {
        counts.push(max);
    }
    counts
}

/// Returns the number of available hardware threads, falling back to 1
/// if the query is unsupported (e.g., inside certain containers/sandboxes).
pub fn max_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Returns thread counts to benchmark based on available parallelism.
///
/// Delegates to [`thread_counts_for`] with the host's core count.
///
/// # Examples
///
/// - 8-core machine:  `[1, 2, 4, 8]`
/// - 24-core machine: `[1, 2, 4, 8, 16, 24]`
/// - 144-core machine: `[1, 2, 4, 8, 16, 32, 64, 128, 144]`
pub fn thread_counts() -> Vec<usize> {
    thread_counts_for(max_threads())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_core() {
        assert_eq!(thread_counts_for(1), vec![1]);
    }

    #[test]
    fn power_of_two() {
        assert_eq!(thread_counts_for(8), vec![1, 2, 4, 8]);
    }

    #[test]
    fn non_power_of_two() {
        assert_eq!(thread_counts_for(24), vec![1, 2, 4, 8, 16, 24]);
    }

    #[test]
    fn large_core_count() {
        assert_eq!(
            thread_counts_for(144),
            vec![1, 2, 4, 8, 16, 32, 64, 128, 144]
        );
    }

    #[test]
    fn zero_is_clamped_to_one() {
        assert_eq!(thread_counts_for(0), vec![1]);
    }

    #[test]
    fn max_threads_is_positive() {
        assert!(max_threads() >= 1);
    }

    #[test]
    fn thread_counts_returns_non_empty() {
        let counts = thread_counts();
        assert!(!counts.is_empty());
        assert_eq!(counts[0], 1);
        assert_eq!(*counts.last().unwrap(), max_threads());
    }
}
