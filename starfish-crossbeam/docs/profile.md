# Profiling Guide

Profile binaries run a single data structure doing a single operation in a tight loop. Attach an external profiler (samply, Instruments, perf, callgrind) to capture stacks. These binaries do **not** produce timing data — use Criterion benchmarks for that (see [benchmarks.md](benchmarks.md)).

## Sorted collections

```
Usage: profile_skiplist <n> <skiplist|skiptrie|crossbeam> <operation> [threads]

Operations:
  seq-insert, random-insert, seq-find, random-find, find-miss,
  mixed-contention, concurrent-insert, concurrent-contention
```

```bash
# Examples
samply record -- cargo run --release --package starfish-crossbeam --bin profile_skiplist -- 500000 skiplist seq-insert
samply record -- cargo run --release --package starfish-crossbeam --bin profile_skiplist -- 500000 skiptrie concurrent-contention 8
samply record -- cargo run --release --package starfish-crossbeam --bin profile_skiplist -- 500000 crossbeam random-find
```

## Hash map collections

```
Usage: profile_hashmap <n> <hashmap|skiplist-pair> <operation> [threads]

Operations:
  seq-insert, seq-find-hit, seq-find-miss, seq-remove, seq-mixed,
  concurrent-insert, concurrent-mixed, concurrent-contention
```

```bash
# Examples
samply record -- cargo run --release --package starfish-crossbeam --bin profile_hashmap -- 500000 hashmap seq-insert
samply record -- cargo run --release --package starfish-crossbeam --bin profile_hashmap -- 500000 skiplist-pair concurrent-mixed 8
```

## Tips

- Use `--release` for realistic profiles (debug builds have very different hotspots)
- Default thread count for concurrent operations is 8; pass `[threads]` to override
- For callgrind: `valgrind --tool=callgrind --callgrind-out-file=callgrind.out cargo run --release --bin profile_skiplist -- 50000 skiplist seq-insert`
- For Instruments (macOS): open Instruments, attach to the process, or use `xcrun xctrace record --template 'Time Profiler' --launch -- cargo run --release ...`
