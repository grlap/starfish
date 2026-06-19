# starfish-storage

Complete transactional storage engine for starfish-db.

## Overview

`starfish-storage` is a self-contained storage engine providing:

- **WAL** - Write-ahead logging for durability and recovery
- **Transactions** - ACID transactions with MVCC
- **Buffer Pool** - In-memory page cache with LRU-K eviction
- **Page Management** - Fixed-size slotted pages for variable-length records
- **Index Structures** - SkipList, Hash, and LSM Tree indexes
- **Storage Backends** - Local disk or remote page server (disaggregated mode)

This crate can be used standalone as an embedded storage engine, similar to RocksDB or SQLite's storage layer.

> **Note**: For distributed deployments, `starfish-wal-server` provides Paxos-based
> consensus for the WAL layer.

## Architecture

```
┌────────────────────────────────────────────────────────────────────┐
│                       starfish-storage                             │
│                                                                    │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                   Transaction Layer                         │   │
│  │         MVCC │ Isolation Levels │ Commit/Abort              │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                     │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                      Index Layer                            │   │
│  │  ┌───────────┐  ┌───────────┐  ┌─────────────────────────┐  │   │
│  │  │ SkipList  │  │   Hash    │  │       LSM Tree          │  │   │
│  │  │  Index    │  │   Index   │  │  ┌───────┐ ┌─────────┐  │  │   │
│  │  │           │  │           │  │  │MemTbl │ │ SSTable │  │  │   │
│  │  └───────────┘  └───────────┘  │  └───────┘ └─────────┘  │  │   │
│  │                                └─────────────────────────┘  │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                     │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                     Buffer Pool                             │   │
│  │            LRU-K eviction │ Page pinning │ Dirty tracking   │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                     │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                         WAL                                 │   │
│  │         Append-only │ Recovery │ Checkpoints                │   │
│  └─────────────────────────────────────────────────────────────┘   │
│                              │                                     │
│  ┌─────────────────────────────────────────────────────────────┐   │
│  │                   Storage Backend                           │   │
│  │         LocalBackend (disk) │ RemoteBackend (page server)   │   │
│  └─────────────────────────────────────────────────────────────┘   │
└────────────────────────────────────────────────────────────────────┘
```

## Module Structure

```
starfish-storage/src/
├── lib.rs
│
├── wal/                      # Write-Ahead Log
│   ├── mod.rs
│   ├── entry.rs              # Log entry types
│   ├── writer.rs             # Append-only writes + fsync
│   ├── reader.rs             # Sequential reads for recovery
│   ├── segment.rs            # Log file segments
│   └── sink.rs               # WalSink trait (local vs distributed)
│
├── transaction/              # Transaction Management
│   ├── mod.rs
│   ├── manager.rs            # Transaction lifecycle
│   ├── mvcc.rs               # Multi-version concurrency control
│   ├── isolation.rs          # Isolation level implementations
│   └── lock.rs               # Lock manager (for serializable)
│
├── page/                     # Page Management
│   ├── mod.rs
│   ├── page_id.rs            # Page addressing (file_id + offset)
│   ├── page.rs               # Fixed-size page (8KB default)
│   └── slotted.rs            # Variable-length record layout
│
├── buffer_pool/              # In-Memory Page Cache
│   ├── mod.rs
│   ├── pool.rs               # LRU-K cache implementation
│   ├── guard.rs              # RAII page pin guard
│   ├── eviction.rs           # Eviction policies
│   └── dirty_tracker.rs      # Modified page tracking
│
├── index/                    # Index Implementations
│   ├── mod.rs
│   ├── traits.rs             # Index trait definitions
│   ├── skiplist.rs           # SkipList index (wraps starfish-core)
│   ├── hash.rs               # Hash index (wraps SplitOrderedHashMap)
│   └── lsm/                  # LSM Tree
│       ├── mod.rs
│       ├── memtable.rs       # In-memory component (SkipList)
│       ├── sstable.rs        # Sorted String Table (immutable)
│       ├── bloom.rs          # Bloom filter for negative lookups
│       ├── compaction.rs     # Background merge strategies
│       ├── manifest.rs       # Level metadata tracking
│       └── iterator.rs       # Merge iterator across levels
│
├── backend/                  # Storage Backend Implementations
│   ├── mod.rs
│   ├── traits.rs             # StorageBackend trait
│   ├── local.rs              # LocalBackend (embedded mode)
│   └── remote.rs             # RemoteBackend (page server client)
│
├── serialization/            # Data Encoding
│   ├── mod.rs
│   └── codec.rs              # rkyv-based serialization
│
└── recovery/                 # Crash Recovery
    ├── mod.rs
    ├── checkpoint.rs         # Checkpoint management
    └── replay.rs             # WAL replay for recovery
```

## WAL (Write-Ahead Log)

Durability and recovery foundation:

```rust
pub struct WalEntry {
    pub lsn: Lsn,               // Log sequence number
    pub prev_lsn: Lsn,          // For undo chaining
    pub txn_id: TxnId,
    pub entry_type: EntryType,
    pub table_id: TableId,
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub old_value: Option<Vec<u8>>,  // For undo/CDC
    pub checksum: u32,
}

pub enum EntryType {
    Begin,
    Commit,
    Abort,
    Insert,
    Update,
    Delete,
    CheckpointBegin,
    CheckpointEnd,
}
```

### WAL Sink Abstraction

Supports both local and distributed WAL:

```rust
#[async_trait]
pub trait WalSink: Send + Sync {
    /// Append entry, returns LSN when durable
    async fn append(&self, entry: WalEntry) -> Result<Lsn>;

    /// Flush up to LSN (durability guarantee)
    async fn flush(&self, lsn: Lsn) -> Result<()>;

    /// Read entries for recovery
    fn read_from(&self, lsn: Lsn) -> impl Iterator<Item = WalEntry>;
}

/// Local file-based WAL
pub struct LocalWalSink { /* ... */ }

/// Distributed WAL via starfish-wal-server
pub struct DistributedWalSink { /* ... */ }
```

### Sync Policies

```rust
pub enum WalSyncPolicy {
    /// Fsync after every write (safest, slowest)
    Immediate,
    /// Fsync at intervals (balanced)
    Batched { interval_ms: u64 },
    /// Let OS decide (fastest, least durable)
    OS,
}
```

## Transaction Layer

ACID transactions with MVCC:

```rust
pub struct Transaction {
    id: TxnId,
    isolation: IsolationLevel,
    snapshot_lsn: Lsn,          // Read snapshot point
    write_set: Vec<WalEntry>,   // Buffered writes
    state: TxnState,
}

pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,  // Default - snapshot isolation
    Serializable,    // SSI (Serializable Snapshot Isolation)
}
```

### Transaction Lifecycle

```
         begin()
            │
            ▼
    ┌───────────────┐
    │    Active     │◄──────────────┐
    └───────┬───────┘               │
            │                       │
    ┌───────┴───────┐               │
    │               │               │
    ▼               ▼               │
 read()          write()            │
    │               │               │
    │               ▼               │
    │      buffer in write_set──────┘
    │
    ├──────────────────────────────┐
    │                              │
    ▼                              ▼
 commit()                       abort()
    │                              │
    ▼                              ▼
 WAL entries                   discard
 flushed                       write_set
```

### MVCC Implementation

Each record has version metadata:

```rust
pub struct VersionedRecord {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub created_txn: TxnId,      // Transaction that created this version
    pub deleted_txn: Option<TxnId>, // Transaction that deleted (if any)
    pub created_lsn: Lsn,
}
```

Visibility rules:
- **Snapshot Isolation**: See records where `created_lsn <= snapshot_lsn` and not deleted
- **Read Committed**: See latest committed version at read time
- **Serializable**: Snapshot + conflict detection at commit

## Index Layer

### Index Selection Guide

| Index Type | Best For | Complexity | Storage |
|------------|----------|------------|---------|
| **SkipList** | Small tables, in-memory, range scans | O(log n) | Memory only |
| **Hash** | Point lookups, consumer offsets | O(1) avg | Memory only |
| **LSM Tree** | Large tables, write-heavy, streams | O(log n) | Memory + Disk |

### SkipList Index

Wrapper around `starfish-core::SkipList`:
- Lock-free concurrent access
- Range scan support
- Best for buffer pool page index, lock manager

```rust
pub struct SkipListIndex<K, V> {
    inner: EpochGuardedCollection<(K, V), SkipListBacklinks<(K, V)>>,
}

impl<K: Ord, V> Index<K, V> for SkipListIndex<K, V> {
    fn get(&self, key: &K) -> Option<V>;
    fn insert(&self, key: K, value: V) -> Option<V>;
    fn remove(&self, key: &K) -> Option<V>;
    fn range(&self, start: &K, end: &K) -> RangeIter<K, V>;
}
```

### Hash Index

Wrapper around `starfish-core::SplitOrderedHashMap`:
- O(1) average lookups
- Best for point queries, consumer group offsets

```rust
pub struct HashIndex<K, V> {
    inner: EpochGuardedHashMap<K, V, SplitOrderedHashMap<K, V>>,
}
```

### LSM Tree

Log-Structured Merge Tree for large, write-heavy workloads:

```
┌─────────────────────────────────────────────────────────────────────┐
│                          LSM Tree                                   │
│                                                                     │
│   Writes ──►  ┌───────────────┐                                     │
│               │   MemTable    │  (SkipList, in-memory)              │
│               │    Active     │                                     │
│               └───────┬───────┘                                     │
│                       │ flush when full                             │
│               ┌───────▼───────┐                                     │
│               │   MemTable    │  (immutable, being flushed)         │
│               │   Immutable   │                                     │
│               └───────┬───────┘                                     │
│                       │                                             │
│   ─────────────────── │ ───────────────── disk boundary ─────────── │
│                       ▼                                             │
│   Level 0   ┌───────┐ ┌───────┐ ┌───────┐                          │
│   (unsorted)│SST-001│ │SST-002│ │SST-003│  (recently flushed)      │
│             └───────┘ └───────┘ └───────┘                          │
│                       │ compaction                                  │
│   Level 1   ┌─────────────────────────────┐                        │
│   (sorted)  │         SST-100             │  (merged, sorted)      │
│             └─────────────────────────────┘                        │
│                       │ compaction                                  │
│   Level 2   ┌─────────────────────────────────────────────┐        │
│   (sorted)  │                 SST-200                     │        │
│             └─────────────────────────────────────────────┘        │
└─────────────────────────────────────────────────────────────────────┘
```

#### LSM Components

**MemTable** - In-memory write buffer
```rust
pub struct MemTable {
    data: SkipListBacklinks<Entry>,
    size: AtomicUsize,
    max_size: usize,  // e.g., 64MB
}
```

**SSTable** - Immutable sorted file on disk
```rust
pub struct SSTable {
    path: PathBuf,
    index: Vec<BlockHandle>,      // Sparse index for binary search
    bloom: BloomFilter,           // Fast negative lookups
    min_key: Vec<u8>,
    max_key: Vec<u8>,
    entry_count: u64,
}
```

**Bloom Filter** - Probabilistic membership test
```rust
pub struct BloomFilter {
    bits: BitVec,
    hash_count: u32,
    // ~1% false positive rate with 10 bits per key
}
```

#### Compaction Strategies

| Strategy | Description | Best For |
|----------|-------------|----------|
| **Leveled** | Merge into sorted levels, size ratio ~10x | General workloads |
| **Tiered** | Group by size, merge similar-sized files | Write-heavy |
| **FIFO** | Delete oldest files, no merging | Streams with TTL |

```rust
pub enum CompactionStrategy {
    /// RocksDB-style leveled compaction
    Leveled { level_size_ratio: usize },

    /// Cassandra-style tiered compaction
    Tiered { min_threshold: usize, max_threshold: usize },

    /// Time-based deletion, no merging (for streams)
    Fifo { ttl: Duration },
}
```

#### LSM for Streams

Streams use FIFO compaction - perfect fit:
- Append-only writes (no updates)
- Time-ordered data
- Retention-based deletion (no merge needed)

```rust
// Stream storage configuration
let stream_lsm = LsmTree::builder()
    .memtable_size(64 * 1024 * 1024)  // 64MB
    .compaction(CompactionStrategy::Fifo {
        ttl: Duration::from_days(7)
    })
    .build()?;
```

## Page Management

### Page Format

Fixed-size pages (default 8KB) with slotted layout:

```
┌──────────────────────────────────────────────────────────┐
│ Page Header (64 bytes)                                   │
│ ┌─────────┬─────────┬───────────┬──────────┬──────────┐ │
│ │   LSN   │Checksum │ Slot Count│Free Start│Free End  │ │
│ └─────────┴─────────┴───────────┴──────────┴──────────┘ │
├──────────────────────────────────────────────────────────┤
│ Slot Directory (grows down)                              │
│ ┌─────────┬─────────┬─────────┬─────────┐               │
│ │ Slot 0  │ Slot 1  │ Slot 2  │   ...   │               │
│ │ off|len │ off|len │ off|len │         │               │
│ └─────────┴─────────┴─────────┴─────────┘               │
├──────────────────────────────────────────────────────────┤
│                    Free Space                            │
├──────────────────────────────────────────────────────────┤
│ Records (grow up)                                        │
│ ┌─────────────┬─────────────┬─────────────┐             │
│ │  Record 2   │  Record 1   │  Record 0   │             │
│ └─────────────┴─────────────┴─────────────┘             │
└──────────────────────────────────────────────────────────┘
```

### Page ID

64-bit identifier combining file and page number:

```rust
#[derive(Copy, Clone, Eq, PartialEq, Hash)]
pub struct PageId(u64);

impl PageId {
    pub fn new(file_id: u32, page_num: u32) -> Self {
        PageId(((file_id as u64) << 32) | (page_num as u64))
    }

    pub fn file_id(&self) -> u32 { (self.0 >> 32) as u32 }
    pub fn page_num(&self) -> u32 { self.0 as u32 }
}
```

## Buffer Pool

In-memory page cache with LRU-K eviction:

```rust
pub struct BufferPool {
    frames: Vec<Frame>,
    page_table: SkipListIndex<PageId, FrameId>,
    eviction: LruK<FrameId>,
    dirty_tracker: DirtyTracker,
}

impl BufferPool {
    /// Get a page for reading
    pub async fn get_page(&self, page_id: PageId) -> Result<PageGuard>;

    /// Get a page for writing (marks dirty)
    pub async fn get_page_mut(&self, page_id: PageId) -> Result<PageGuardMut>;

    /// Flush dirty pages to storage
    pub async fn flush(&self) -> Result<()>;
}
```

### LRU-K Eviction

Tracks last K accesses to resist scan pollution:

```rust
pub struct LruK<T> {
    k: usize,  // typically 2
    history: HashMap<T, VecDeque<Instant>>,
    // Evict page with oldest K-th access time
}
```

## Storage Backend

Abstracts local vs remote storage:

```rust
#[async_trait]
pub trait StorageBackend: Send + Sync {
    /// Read a page
    async fn read_page(&self, page_id: PageId) -> Result<Page>;

    /// Write a page
    async fn write_page(&self, page_id: PageId, page: &Page) -> Result<()>;

    /// Allocate new page
    async fn allocate_page(&self) -> Result<PageId>;

    /// Sync to durable storage
    async fn sync(&self) -> Result<()>;
}
```

### Local Backend

Direct disk I/O for embedded mode:

```rust
pub struct LocalBackend {
    files: HashMap<u32, File>,
    page_size: usize,
}
```

### Remote Backend

Page server client for disaggregated mode:

```rust
pub struct RemoteBackend {
    endpoints: Vec<PageServerEndpoint>,
    // Routes requests based on page range sharding
}
```

## Design Decisions

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Page size | 8KB | Balance of I/O efficiency and fragmentation |
| Eviction | LRU-K (K=2) | Scan-resistant, proven in databases |
| LSM MemTable | SkipList | Reuse existing lock-free implementation |
| Bloom filter | 10 bits/key | ~1% false positive rate |
| Serialization | rkyv | Zero-copy deserialization |

## Dependencies

- `starfish-core` - SkipList, SplitOrderedHashMap
- `starfish-crossbeam` - Epoch-based memory reclamation
- `starfish-reactor` - Async I/O
- `starfish-wal` - WAL integration (separate crate)
- `rkyv` - Zero-copy serialization
- `crc32fast` - Checksums

## References

- [LSM Tree Paper](https://www.cs.umb.edu/~poneil/lsmtree.pdf)
- [RocksDB Compaction](https://github.com/facebook/rocksdb/wiki/Compaction)
- [LevelDB Implementation](https://github.com/google/leveldb/blob/main/doc/impl.md)
- [Bloom Filters by Example](https://llimllib.github.io/bloomfilter-tutorial/)
- [Buffer Pool Management (CMU)](https://15445.courses.cs.cmu.edu/fall2022/slides/06-bufferpool.pdf)
