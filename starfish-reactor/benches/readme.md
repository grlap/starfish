## Setup

```bash
cargo install flamegraph
sudo apt install valgrind
sudo apt install kcachegrind
```

## Profile

```bash
valgrind --tool=callgrind target/release/deps/my_benchmark-
```

## View results

kcachegrind callgrind.out.*

