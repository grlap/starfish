//! Connection pool for HTTP clients.
//!
//! [`ConnectionPool`] manages reusable HTTP/1.1 connections keyed by
//! `(host, port, is_tls)`. HTTP/3 connections are not pooled here
//! since QUIC handles multiplexing natively.

use std::collections::HashMap;

/// Key for connection pool lookups.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolKey {
    pub host: String,
    pub port: u16,
    pub is_tls: bool,
    pub proxy: Option<String>,
}

/// A connection pool for HTTP/1.1 connections.
///
/// Stores idle connections for reuse. Connections are checked out by key
/// and returned after use if still reusable (keep-alive).
pub struct ConnectionPool<C> {
    conns: HashMap<PoolKey, Vec<C>>,
    max_idle_per_host: usize,
}

impl<C> ConnectionPool<C> {
    /// Create a new pool with the given max idle connections per host.
    pub fn new(max_idle_per_host: usize) -> Self {
        Self {
            conns: HashMap::new(),
            max_idle_per_host,
        }
    }

    /// Try to take an idle connection for the given key.
    pub fn take(&mut self, key: &PoolKey) -> Option<C> {
        self.conns.get_mut(key)?.pop()
    }

    /// Return a connection to the pool for reuse.
    pub fn put(&mut self, key: PoolKey, conn: C) {
        let entry = self.conns.entry(key).or_default();
        if entry.len() < self.max_idle_per_host {
            entry.push(conn);
        }
        // else: drop the connection (pool full)
    }

    /// Remove all idle connections for the given key.
    pub fn remove(&mut self, key: &PoolKey) {
        self.conns.remove(key);
    }

    /// Clear all idle connections.
    pub fn clear(&mut self) {
        self.conns.clear();
    }

    /// Number of keys with idle connections.
    pub fn len(&self) -> usize {
        self.conns.len()
    }

    /// Whether the pool is empty.
    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_put_and_take() {
        let mut pool: ConnectionPool<String> = ConnectionPool::new(2);
        let key = PoolKey {
            host: "example.com".into(),
            port: 443,
            is_tls: true,
            proxy: None,
        };

        assert!(pool.take(&key).is_none());

        pool.put(key.clone(), "conn1".into());
        pool.put(key.clone(), "conn2".into());

        assert_eq!(pool.take(&key), Some("conn2".into()));
        assert_eq!(pool.take(&key), Some("conn1".into()));
        assert!(pool.take(&key).is_none());
    }

    #[test]
    fn pool_max_idle() {
        let mut pool: ConnectionPool<String> = ConnectionPool::new(1);
        let key = PoolKey {
            host: "example.com".into(),
            port: 80,
            is_tls: false,
            proxy: None,
        };

        pool.put(key.clone(), "conn1".into());
        pool.put(key.clone(), "conn2".into()); // should be dropped

        assert_eq!(pool.take(&key), Some("conn1".into()));
        assert!(pool.take(&key).is_none());
    }

    #[test]
    fn pool_remove() {
        let mut pool: ConnectionPool<String> = ConnectionPool::new(4);
        let key = PoolKey {
            host: "a.com".into(),
            port: 80,
            is_tls: false,
            proxy: None,
        };
        pool.put(key.clone(), "c1".into());
        pool.put(key.clone(), "c2".into());
        assert_eq!(pool.len(), 1); // one host

        pool.remove(&key);
        assert!(pool.take(&key).is_none());
        assert!(pool.is_empty());
    }

    #[test]
    fn pool_clear() {
        let mut pool: ConnectionPool<String> = ConnectionPool::new(4);
        let k1 = PoolKey {
            host: "a.com".into(),
            port: 80,
            is_tls: false,
            proxy: None,
        };
        let k2 = PoolKey {
            host: "b.com".into(),
            port: 443,
            is_tls: true,
            proxy: None,
        };
        pool.put(k1.clone(), "c1".into());
        pool.put(k2.clone(), "c2".into());
        assert_eq!(pool.len(), 2);
        assert!(!pool.is_empty());

        pool.clear();
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
        assert!(pool.take(&k1).is_none());
        assert!(pool.take(&k2).is_none());
    }

    #[test]
    fn pool_len_and_is_empty() {
        let pool: ConnectionPool<String> = ConnectionPool::new(4);
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
    }
}
