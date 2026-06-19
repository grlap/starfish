# starfish-db

Transactional database with integrated stream processing, built on the starfish async runtime.

## Overview

`starfish-db` is a high-performance database designed for applications requiring:

- **ACID transactions** with configurable isolation levels
- **Integrated stream processing** - read, process, and commit atomically (exactly-once semantics)
- **Kafka-compatible protocol** - use existing Kafka clients
- **Disaggregated architecture** - separate compute and storage for scalability
- **Lock-free indexing** leveraging `starfish-core` data structures
- **Cross-platform async I/O** via `starfish-reactor`

## Deployment Modes

### Mode 1: Local (Embedded)

Single-process deployment for development or embedded use cases.

```
┌─────────────────────────────────────────────────────────────────┐
│                        starfish-db                              │
│  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────────────┐       │
│  │ Compute │ │   WAL   │ │  Pages  │ │  Buffer Pool    │       │
│  └─────────┘ └─────────┘ └─────────┘ └─────────────────┘       │
│                      ▼ Local Disk                               │
└─────────────────────────────────────────────────────────────────┘
```

### Mode 2: Disaggregated (Scalable)

Separate compute and storage for horizontal scaling.

```
┌──────────────────────────────────────────────────────────────────────────┐
│                                                                          │
│  ┌──────────────────────┐                                                │
│  │   Compute Node(s)    │        ┌─────────────────────────────┐       │
│  │  ┌─────────────────┐ │        │      WAL Servers (Paxos)    │       │
│  │  │  Query/Txn      │ │───────►│  ┌──────┐┌──────┐┌──────┐   │       │
│  │  │  Buffer Pool    │ │  WAL   │  │ WAL1 ││ WAL2 ││ WAL3 │   │       │
│  │  │  (cache only)   │ │        │  └──┬───┘└──┬───┘└──┬───┘   │       │
│  │  └─────────────────┘ │        │     └───────┼───────┘       │       │
│  └──────────────────────┘        │        Consensus            │       │
│         │ │ │                    └─────────────┬───────────────┘       │
│    Multiple compute                            │                        │
│    nodes supported                             │ WAL Stream             │
│                                                ▼                        │
│                           ┌──────────────────────────────────────┐     │
│                           │         Page Server(s)               │     │
│                           │  ┌────────────┐  ┌────────────┐      │     │
│                           │  │   Pages    │  │    WAL     │      │     │
│                           │  │  Storage   │  │   Replay   │      │     │
│                           │  └────────────┘  └────────────┘      │     │
│                           └──────────────────────────────────────┘     │
│                                                                          │
└──────────────────────────────────────────────────────────────────────────┘
```

**Why WAL Servers?**
- Optimized for low-latency consensus (Paxos)
- Lightweight - just append-only log
- Can deploy on NVMe-optimized machines
- Decoupled from heavy page reconstruction work

**Page Flow:**
1. Compute nodes request pages from Page Servers (pull model)
2. Page Servers reconstruct pages by replaying WAL from last checkpoint
3. Page Servers cache reconstructed pages for subsequent requests
4. Compute nodes cache pages locally in their buffer pools

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Client API                               │
│  (Database → Table → Transaction → Stream)                      │
├─────────────────────────────────────────────────────────────────┤
│                   Stream Protocol Layer                         │
│  (Kafka protocol compatibility)                                 │
├─────────────────────────────────────────────────────────────────┤
│                    Transaction Layer                            │
│  (TransactionManager → MVCC → Streams as Tables)                │
├─────────────────────────────────────────────────────────────────┤
│                      Index Layer                                │
│  (SkipListIndex │ HashIndex │ BPlusTreeIndex)                   │
├─────────────────────────────────────────────────────────────────┤
│                     Storage Layer                               │
│  (BufferPool → PageManager → StorageBackend)                    │
├─────────────────────────────────────────────────────────────────┤
│                      WAL Layer                                  │
│  (WAL Servers with Paxos │ Local WAL)                           │
├─────────────────────────────────────────────────────────────────┤
│                   I/O Foundation                                │
│  (starfish-reactor: kqueue / io_uring / IOCP)                   │
└─────────────────────────────────────────────────────────────────┘
```

## Modules

`starfish-db` currently has a single top-level module (`lib.rs`) that serves as the assembly crate, re-exporting and orchestrating the component crates below.

## Component Crates

```
                         ┌─────────────────────┐
                         │    starfish-db      │  ← Final assembly, high-level API
                         └─────────┬───────────┘
                                   │
               ┌───────────────────┼───────────────────┐
               │                   │                   │
               ▼                   ▼                   ▼
      ┌─────────────────┐ ┌───────────────┐ ┌─────────────────┐
      │ starfish-stream │ │starfish-query │ │starfish-wal-    │
      │ (Kafka protocol)│ │  (future)     │ │server (Paxos)   │
      └────────┬────────┘ └───────────────┘ └────────┬────────┘
               │                                     │
               └──────────────────┬──────────────────┘
                                  │
                                  ▼
                    ┌─────────────────────────┐
                    │    starfish-storage     │  ← Complete storage engine
                    │  ┌───────────────────┐  │
                    │  │ WAL, Transactions │  │
                    │  │ MVCC, Recovery    │  │
                    │  │ Buffer Pool       │  │
                    │  │ Pages, Indexes    │  │
                    │  │ LSM Tree          │  │
                    │  └───────────────────┘  │
                    └─────────────────────────┘
```

| Crate | Purpose |
|-------|---------|
| `starfish-db` | Final assembly - high-level Database/Table API |
| `starfish-storage` | **Complete storage engine**: WAL, transactions, MVCC, buffer pool, pages, indexes, LSM |
| `starfish-stream` | Kafka-compatible protocol, stream semantics on top of storage |
| `starfish-wal-server` | Distributed WAL servers with Paxos (for disaggregated mode only) |
| `starfish-query` | SQL parsing & execution (future) |

## Stream Processing

### Streams are Transactional Tables

Streams in starfish-db are first-class citizens backed by the same transactional engine:

- **Append-only tables** with partition keys
- **Consumer groups** track offsets (stored as regular table rows)
- **Exactly-once semantics** via single transaction commit

### Transactional Stream Processing Example

```rust
use starfish_db::{Database, IsolationLevel};

let db = Database::open("my_data", Options::default()).await?;

// Start transaction
let txn = db.begin(IsolationLevel::RepeatableRead).await?;

// 1. Read from input stream (consumer group tracks offset)
let records = txn.stream("orders")
    .consumer_group("order-processor")
    .poll(100)
    .await?;

// 2. Process records
for record in records {
    let order: Order = record.value()?;
    let invoice = process_order(&order);

    // Write to regular table
    txn.table("invoices").insert(&invoice.id, &invoice).await?;

    // Write to output stream
    txn.stream("invoices-stream").send(&invoice.id, &invoice).await?;
}

// 3. Atomic commit: input offset + table writes + output stream
//    All or nothing - exactly-once semantics
txn.commit().await?;
```

### What Gets Committed Atomically

```
┌─────────────────────────────────────────────────────────────────────┐
│                     Single WAL Entry                                │
│                                                                     │
│  LSN: 5001                                                          │
│  TxnId: 42                                                          │
│  Operations:                                                        │
│    - UPDATE consumer_offsets                                        │
│        SET offset = 1043                                            │
│        WHERE group = 'order-processor' AND stream = 'orders'        │
│    - INSERT INTO invoices (id, data) VALUES (...)                   │
│    - INSERT INTO invoices_stream (partition, key, value) VALUES     │
│                                                                     │
│  ──────────────────────────────────────────────────────────────     │
│  All operations in ONE atomic commit                                │
└─────────────────────────────────────────────────────────────────────┘
```

## Core Components

### Storage Engine (`starfish-storage`)

Complete transactional storage engine:

**WAL (Write-Ahead Log)**
- Append-only durability log
- Log segments with checksums
- Recovery and crash safety
- Configurable sync policies

**Transactions & MVCC**
- ACID transaction support
- Multi-Version Concurrency Control
- Snapshot isolation (default)
- SSI for serializable

| Isolation Level | Description |
|-----------------|-------------|
| `ReadUncommitted` | No isolation (dirty reads allowed) |
| `ReadCommitted` | See only committed data |
| `RepeatableRead` | Snapshot at transaction start |
| `Serializable` | Full serializability via SSI |

**Buffer Pool & Pages**
- LRU-K page cache with pinning
- Slotted pages for variable-length records
- Dirty page tracking and background flush

**Indexes**
- SkipList (lock-free, range scans)
- Hash (O(1) point lookups)
- LSM Tree (write-heavy, large datasets)

### WAL Server (`starfish-wal-server`)

For disaggregated mode only:
- **Paxos consensus** across WAL server cluster
- **WAL streaming** to page servers
- Optimized for NVMe, low-latency writes

### Stream Layer (`starfish-stream`)

Kafka-compatible streaming:
- **Kafka protocol subset** - Produce, Fetch, Offsets (v0-v3)
- **Partitioned topics** - scalable parallel processing
- **Consumer groups** - coordinated consumption
- **Exactly-once** - via transactional commits

Note: Initial implementation targets a minimal Kafka protocol subset sufficient for
basic produce/consume operations. Full Kafka API compatibility is not a goal.

## Design Decisions

| Aspect | Choice | Rationale |
|--------|--------|-----------|
| WAL consensus | Paxos | Proven, low-latency for write path |
| WAL servers | Dedicated | Separate from page servers for latency |
| Page versioning | Hybrid | Recent versions cached, old from WAL |
| Streams | As tables | Unified transactional model |
| Protocol | Kafka-compatible | Ecosystem compatibility |
| Serialization | rkyv | Zero-copy deserialization |

## Implementation Phases

### Phase 0: Crate Scaffolding
- [ ] Create `starfish-storage` module structure
- [ ] Create `starfish-stream` crate skeleton
- [ ] Create `starfish-wal-server` crate skeleton
- [ ] Establish dependency graph between crates

### Phase 1: WAL Foundation (`starfish-storage`)
- [ ] WAL entry format and serialization (rkyv)
- [ ] Log segment management with checksums
- [ ] Local WAL writer with configurable sync
- [ ] WAL reader and recovery logic
- [ ] Checkpoint support for faster recovery

### Phase 2: Buffer Pool & Pages (`starfish-storage`)
- [ ] Slotted page format for variable-length records
- [ ] Page allocation and free list
- [ ] Buffer pool with LRU-K eviction
- [ ] Dirty page tracking and flush
- [ ] Pin/unpin semantics

### Phase 3: Transaction Support (`starfish-storage`)
- [ ] Transaction manager (begin/commit/abort)
- [ ] MVCC version chains
- [ ] Snapshot isolation (default)
- [ ] Read committed isolation
- [ ] SSI for serializable (conflict detection)

### Phase 4: Indexes (`starfish-storage`)
- [ ] SkipList index (wrapping starfish-core)
- [ ] Hash index for point lookups
- [ ] LSM Tree for write-heavy workloads
- [ ] Compaction strategies

### Phase 5: Stream Integration (`starfish-stream`)
- [ ] Stream table format (append-only)
- [ ] Consumer group offset tracking
- [ ] Transactional stream operations
- [ ] Kafka protocol subset (Produce, Fetch, Offsets - v0-v3)

### Phase 6: Distributed Mode (`starfish-wal-server`)
- [ ] WAL server with Paxos consensus
- [ ] WAL streaming to page servers
- [ ] Remote storage backend
- [ ] Compute node implementation

### Phase 7: Query Engine (Future - `starfish-query`)
- [ ] SQL parser integration
- [ ] Query planner
- [ ] Execution operators

## Performance Targets

| Operation | Target | Notes |
|-----------|--------|-------|
| Point read | < 10μs | Hot data in buffer pool |
| Point write | < 50μs | Excludes consensus |
| Committed write | < 5ms | With Paxos (3 nodes) |
| Stream consume | 100K msg/sec | Per partition |
| Exactly-once txn | < 10ms | Read + process + commit |

## Dependencies

- `starfish-core` - Lock-free data structures
- `starfish-reactor` - Async I/O runtime
- `starfish-crossbeam` - Epoch-based memory reclamation
- `rkyv` - Zero-copy serialization
- `crc32fast` - Checksum for WAL integrity

## References

- [Amazon Aurora: Design Considerations](https://www.allthingsdistributed.com/files/p1041-verbitski.pdf)
- [Neon: Serverless Postgres Architecture](https://neon.tech/docs/introduction/architecture-overview)
- [Kafka Exactly-Once Semantics](https://www.confluent.io/blog/exactly-once-semantics-are-possible-heres-how-apache-kafka-does-it/)
- [ARIES Recovery Algorithm](https://cs.stanford.edu/people/chr101/cs345/aries.pdf)
- [Architecture of a Database System](https://dsf.berkeley.edu/papers/fntdb07-architecture.pdf)
