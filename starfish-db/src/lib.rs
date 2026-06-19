//! # starfish-db
//!
//! Transactional database with integrated stream processing, built on the starfish async runtime.
//!
//! This crate composes several component crates:
//! - `starfish-storage` - Complete storage engine: WAL, transactions, MVCC, buffer pool, pages, indexes
//! - `starfish-stream` - Kafka-compatible protocol, stream semantics on top of storage
//! - `starfish-wal-server` - Distributed WAL servers with Paxos (for disaggregated mode)
//! - `starfish-query` - SQL parsing & execution (future)
//!
//! See README.md for full architecture documentation.

#![forbid(unsafe_op_in_unsafe_fn)]

// Component crates will be re-exported here once implemented:
// pub use starfish_storage as storage;
// pub use starfish_stream as stream;
// pub use starfish_wal_server as wal_server;
// pub use starfish_query as query;
