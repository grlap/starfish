//! Treap: a randomized binary search tree combining BST and heap properties.
//!
//! Provides a sequential (non-lock-free) key-value map ordered by keys,
//! with expected O(log n) operations via random priorities.

use std::cmp::Ordering;

/// Node structure for the Treap.
///
struct TreapNode<K: Ord + Clone, V, P: Ord + Clone> {
    key: K,
    value: V,
    priority: P,
    left: Option<Box<TreapNode<K, V, P>>>,
    right: Option<Box<TreapNode<K, V, P>>>,
}

/// Treap structure.
///
pub struct Treap<K: Ord + Clone, V, P: Ord + Clone> {
    root: Option<Box<TreapNode<K, V, P>>>,
}

impl<K: Ord + Clone, V, P: Ord + Clone> TreapNode<K, V, P> {
    fn new(key: K, value: V, priority: P) -> Self {
        TreapNode {
            key,
            value,
            priority,
            left: None,
            right: None,
        }
    }
}

impl<K: Ord + Clone, V, P: Ord + Clone> Treap<K, V, P> {
    /// Creates a new empty Treap.
    ///
    pub fn new() -> Self {
        Treap { root: None }
    }

    // Checks if the treap contains a key.
    //
    pub fn contains(&self, key: &K) -> bool {
        Self::contains_rec(&self.root, key)
    }

    fn contains_rec(node: &Option<Box<TreapNode<K, V, P>>>, key: &K) -> bool {
        match node {
            None => false,
            Some(node) => match key.cmp(&node.key) {
                Ordering::Equal => true,
                Ordering::Less => Self::contains_rec(&node.left, key),
                Ordering::Greater => Self::contains_rec(&node.right, key),
            },
        }
    }

    /// Gets a reference to the value associated with a key.
    ///
    pub fn get(&self, key: &K) -> Option<&V> {
        Self::get_rec(&self.root, key)
    }

    fn get_rec<'a>(node: &'a Option<Box<TreapNode<K, V, P>>>, key: &K) -> Option<&'a V> {
        match node {
            None => None,
            Some(node) => match key.cmp(&node.key) {
                Ordering::Equal => Some(&node.value),
                Ordering::Less => Self::get_rec(&node.left, key),
                Ordering::Greater => Self::get_rec(&node.right, key),
            },
        }
    }

    /// Inserts a key-value pair with a specified priority.
    ///
    pub fn insert(&mut self, key: K, value: V, priority: P) {
        self.root = Self::insert_rec(self.root.take(), key, value, priority);
    }

    fn insert_rec(
        node: Option<Box<TreapNode<K, V, P>>>,
        key: K,
        value: V,
        priority: P,
    ) -> Option<Box<TreapNode<K, V, P>>> {
        match node {
            None => Some(Box::new(TreapNode::new(key, value, priority))),
            Some(mut node_box) => {
                match key.cmp(&node_box.key) {
                    Ordering::Equal => {
                        // Key already exists, update value and priority
                        node_box.value = value;
                        node_box.priority = priority;
                        Some(node_box)
                    }
                    Ordering::Less => {
                        // Insert to the left subtree.
                        //
                        node_box.left =
                            Self::insert_rec(node_box.left.take(), key, value, priority);

                        // Rotate if heap property is violated.
                        //
                        if let Some(left) = &node_box.left
                            && left.priority < node_box.priority
                        {
                            return Self::rotate_right(Some(node_box));
                        }

                        Some(node_box)
                    }
                    Ordering::Greater => {
                        // Insert to the right subtree
                        node_box.right =
                            Self::insert_rec(node_box.right.take(), key, value, priority);

                        // Rotate if heap property is violated
                        if let Some(right) = &node_box.right
                            && right.priority < node_box.priority
                        {
                            return Self::rotate_left(Some(node_box));
                        }

                        Some(node_box)
                    }
                }
            }
        }
    }

    // Remove a node from the Treap by a given key.
    //
    pub fn remove(&mut self, key: &K) -> Option<V> {
        let mut result = None;
        self.root = Self::remove_rec(self.root.take(), key, &mut result);
        result
    }

    fn remove_rec(
        node: Option<Box<TreapNode<K, V, P>>>,
        key: &K,
        result: &mut Option<V>,
    ) -> Option<Box<TreapNode<K, V, P>>> {
        match node {
            None => None,
            Some(mut node_box) => {
                match key.cmp(&node_box.key) {
                    Ordering::Less => {
                        node_box.left = Self::remove_rec(node_box.left.take(), key, result);
                        Some(node_box)
                    }
                    Ordering::Greater => {
                        node_box.right = Self::remove_rec(node_box.right.take(), key, result);
                        Some(node_box)
                    }
                    Ordering::Equal => {
                        // Case 1: Leaf node
                        if node_box.left.is_none() && node_box.right.is_none() {
                            *result = Some(node_box.value);
                            return None;
                        }

                        // Case 2: Only one child
                        if node_box.left.is_none() {
                            *result = Some(node_box.value);
                            return node_box.right;
                        }

                        if node_box.right.is_none() {
                            *result = Some(node_box.value);
                            return node_box.left;
                        }

                        // Case 3: Two children - rotate the node down until it
                        // becomes a leaf or single-child, then remove it.
                        // This maintains the min-heap invariant on priorities.
                        if node_box.left.as_ref().unwrap().priority
                            < node_box.right.as_ref().unwrap().priority
                        {
                            // Left child has lower priority, rotate right
                            let mut rotated = Self::rotate_right(Some(node_box)).unwrap();
                            // The target key is now in rotated.right, recurse
                            rotated.right = Self::remove_rec(rotated.right.take(), key, result);
                            Some(rotated)
                        } else {
                            // Right child has lower or equal priority, rotate left
                            let mut rotated = Self::rotate_left(Some(node_box)).unwrap();
                            // The target key is now in rotated.left, recurse
                            rotated.left = Self::remove_rec(rotated.left.take(), key, result);
                            Some(rotated)
                        }
                    }
                }
            }
        }
    }

    // Gets the node with the lowest priority.
    //
    pub fn get_min_priority_node(&self) -> Option<(&K, &V, &P)> {
        self.root.as_ref()?;

        // First find the key with minimum priority.
        //
        let root = self.root.as_ref().unwrap();
        Some((&root.key, &root.value, &root.priority))
    }

    // Rotate right operation for tree balancing.
    //
    fn rotate_right(node: Option<Box<TreapNode<K, V, P>>>) -> Option<Box<TreapNode<K, V, P>>> {
        if let Some(mut root) = node {
            if let Some(mut left) = root.left.take() {
                root.left = left.right.take();
                left.right = Some(root);
                return Some(left);
            }
            Some(root)
        } else {
            None
        }
    }

    // Rotate left operation for tree balancing.
    //
    fn rotate_left(node: Option<Box<TreapNode<K, V, P>>>) -> Option<Box<TreapNode<K, V, P>>> {
        if let Some(mut root) = node {
            if let Some(mut right) = root.right.take() {
                root.right = right.left.take();
                right.left = Some(root);
                return Some(right);
            }
            Some(root)
        } else {
            None
        }
    }

    // In-order traversal for debugging.
    //
    pub fn in_order_traversal(&self, visit: impl Fn(&K, &V, &P)) {
        Self::in_order_traversal_rec(&self.root, &visit);
    }

    fn in_order_traversal_rec(node: &Option<Box<TreapNode<K, V, P>>>, visit: &impl Fn(&K, &V, &P)) {
        if let Some(node) = node {
            Self::in_order_traversal_rec(&node.left, visit);
            visit(&node.key, &node.value, &node.priority);
            Self::in_order_traversal_rec(&node.right, visit);
        }
    }
}

impl<K: Ord + Clone, V, P: Ord + Clone> Default for Treap<K, V, P> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify both BST ordering and min-heap priority invariants for the entire tree.
    fn assert_invariants<K: Ord + Clone + std::fmt::Debug, V, P: Ord + Clone + std::fmt::Debug>(
        treap: &Treap<K, V, P>,
    ) {
        fn check<K: Ord + Clone + std::fmt::Debug, V, P: Ord + Clone + std::fmt::Debug>(
            node: &Option<Box<TreapNode<K, V, P>>>,
            min_key: Option<&K>,
            max_key: Option<&K>,
            parent_priority: Option<&P>,
        ) {
            let Some(n) = node else { return };

            // BST property: min_key < n.key < max_key
            if let Some(min) = min_key {
                assert!(
                    n.key > *min,
                    "BST violation: {:?} should be > {:?}",
                    n.key,
                    min
                );
            }
            if let Some(max) = max_key {
                assert!(
                    n.key < *max,
                    "BST violation: {:?} should be < {:?}",
                    n.key,
                    max
                );
            }

            // Min-heap property: parent.priority <= child.priority
            if let Some(pp) = parent_priority {
                assert!(
                    n.priority >= *pp,
                    "Heap violation: node {:?} priority {:?} < parent priority {:?}",
                    n.key,
                    n.priority,
                    pp
                );
            }

            check(&n.left, min_key, Some(&n.key), Some(&n.priority));
            check(&n.right, Some(&n.key), max_key, Some(&n.priority));
        }

        check(&treap.root, None, None, None);
    }

    #[test]
    fn verify_min_priority_node() {
        let mut treap = Treap::new();

        assert_eq!(None, treap.get_min_priority_node());

        treap.insert(5, "five", 50);
        assert_eq!(Some((&5, &"five", &50)), treap.get_min_priority_node());

        treap.insert(2, "two", 20);
        assert_eq!(Some((&2, &"two", &20)), treap.get_min_priority_node());

        treap.insert(7, "seven", 70);
        assert_eq!(Some((&2, &"two", &20)), treap.get_min_priority_node());

        treap.insert(1, "one", 10);
        assert_eq!(Some((&1, &"one", &10)), treap.get_min_priority_node());

        treap.insert(4, "four", 40);
        assert_eq!(Some((&1, &"one", &10)), treap.get_min_priority_node());

        assert!(treap.contains(&5));
        assert!(!treap.contains(&6));
        assert_invariants(&treap);

        // Remove min-priority element, verify invariants hold
        let (key, _, _) = treap.get_min_priority_node().unwrap();
        assert_eq!(&1, key);
        assert_eq!(Some("one"), treap.remove(&key.clone()));
        assert_invariants(&treap);

        let (key, _, _) = treap.get_min_priority_node().unwrap();
        assert_eq!(&2, key);
        assert_eq!(Some("two"), treap.remove(&key.clone()));
        assert_invariants(&treap);
    }

    #[test]
    fn remove_twice() {
        let mut treap = Treap::new();

        treap.insert(5, "five", 50);
        treap.insert(2, "two", 20);
        treap.insert(7, "seven", 70);

        assert_eq!(Some("five"), treap.remove(&5));
        assert_eq!(None, treap.remove(&5));
        assert_invariants(&treap);
    }

    #[test]
    fn verify_contains() {
        let mut treap = Treap::new();

        treap.insert(1, "one", 1);
        treap.insert(2, "two", 2);
        treap.insert(3, "three", 3);

        assert!(treap.contains(&2));
        assert!(!treap.contains(&5));
    }

    #[test]
    fn verify_gets() {
        let mut treap = Treap::new();

        treap.insert(100, "100", 100);
        treap.insert(200, "200", 200);
        treap.insert(300, "300", 300);

        assert_eq!(Some(&"200"), treap.get(&200));
    }

    #[test]
    fn remove_root_with_two_children() {
        let mut treap = Treap::new();

        // Build a tree where root has two children
        treap.insert(5, "five", 10); // root (lowest priority)
        treap.insert(3, "three", 30);
        treap.insert(7, "seven", 20);
        treap.insert(1, "one", 50);
        treap.insert(4, "four", 40);
        treap.insert(6, "six", 60);
        treap.insert(9, "nine", 35);
        assert_invariants(&treap);

        // Remove root - this exercises Case 3 (two children)
        assert_eq!(Some("five"), treap.remove(&5));
        assert_invariants(&treap);
        assert!(!treap.contains(&5));

        // All other keys still present
        assert!(treap.contains(&3));
        assert!(treap.contains(&7));
        assert!(treap.contains(&1));
        assert!(treap.contains(&4));
        assert!(treap.contains(&6));
        assert!(treap.contains(&9));

        // Min priority should now be 7/seven (priority 20)
        let (key, _, priority) = treap.get_min_priority_node().unwrap();
        assert_eq!(&7, key);
        assert_eq!(&20, priority);
    }

    #[test]
    fn remove_all_elements() {
        let mut treap = Treap::new();

        treap.insert(5, 5, 50);
        treap.insert(3, 3, 30);
        treap.insert(8, 8, 80);
        treap.insert(1, 1, 10);
        treap.insert(4, 4, 40);
        treap.insert(7, 7, 70);
        treap.insert(9, 9, 90);

        // Remove all in an order that forces various rotation patterns
        for &key in &[5, 1, 9, 3, 7, 4, 8] {
            assert_eq!(Some(key), treap.remove(&key));
            assert_invariants(&treap);
            assert!(!treap.contains(&key));
        }

        assert_eq!(None, treap.get_min_priority_node());
    }
}
