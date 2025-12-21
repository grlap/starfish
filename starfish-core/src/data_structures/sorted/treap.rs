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

                        // Case 3: Two children - find a replacement node based on priority
                        if node_box.left.as_ref().unwrap().priority
                            < node_box.right.as_ref().unwrap().priority
                        {
                            // Left child has higher priority, it becomes the new root
                            // Take ownership of the children to manipulate them
                            let mut left_child = node_box.left.take().unwrap();
                            let old_right = node_box.right.take();

                            // Store the result value before dropping node_box
                            *result = Some(node_box.value);

                            // Take right subtree of left_child
                            let left_right = left_child.right.take();

                            // Construct the right subtree - the original right and the right of left_child
                            if let Some(lr) = left_right {
                                // We have a right subtree from the left child
                                // Create a new right subtree by "joining" the two subtrees

                                // Perform a simple join (this is a simplified approach)
                                let mut right_tree = old_right;
                                let mut current = &mut right_tree;

                                // Find leftmost position to insert
                                while let Some(ref mut node) = *current {
                                    current = &mut node.left;
                                }

                                // Insert the left's right subtree
                                *current = Some(lr);

                                left_child.right = right_tree;
                            } else {
                                // No right subtree from left child, just use the original right
                                left_child.right = old_right;
                            }

                            Some(left_child)
                        } else {
                            // Right child has higher or equal priority, it becomes the new root
                            // Take ownership of the children to manipulate them
                            let mut right_child = node_box.right.take().unwrap();
                            let old_left = node_box.left.take();

                            // Store the result value before dropping node_box
                            *result = Some(node_box.value);

                            // Take left subtree of right_child
                            let right_left = right_child.left.take();

                            // Construct the left subtree - the original left and the left of right_child
                            if let Some(rl) = right_left {
                                // We have a left subtree from the right child
                                // Create a new left subtree by "joining" the two subtrees

                                // Perform a simple join (this is a simplified approach)
                                let mut left_tree = old_left;
                                let mut current = &mut left_tree;

                                // Find rightmost position to insert
                                while let Some(ref mut node) = *current {
                                    current = &mut node.right;
                                }

                                // Insert the right's left subtree
                                *current = Some(rl);

                                right_child.left = left_tree;
                            } else {
                                // No left subtree from right child, just use the original left
                                right_child.left = old_left;
                            }

                            Some(right_child)
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

    #[test]
    fn verify_min_priority_node() {
        let mut treap = Treap::new();

        assert_eq!(None, treap.get_min_priority_node());

        // Insert some values with custom priorities.
        //
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

        // Verify contains.
        //
        assert_eq!(true, treap.contains(&5));
        assert_eq!(false, treap.contains(&6));

        // Display in-order traversal with priorities.
        //
        println!("In-order traversal:");
        treap.in_order_traversal(|key, value, priority| {
            println!("Key: {}, Value: {}, Priority: {}", key, value, priority)
        });

        // Remove element with minimum priority (returns both key and value).
        //
        let (key, _, _) = treap.get_min_priority_node().unwrap();
        assert_eq!(&1, key);
        assert_eq!(Some("one"), treap.remove(&key.clone()));

        // Remove another element with minimum priority (returns only the value).
        //
        let (key, _, _) = treap.get_min_priority_node().unwrap();
        assert_eq!(&2, key);
        assert_eq!(Some("two"), treap.remove(&key.clone()));

        // Display after removal
        println!("After removing min priority elements:");
        treap.in_order_traversal(|key, value, priority| {
            println!("Key: {}, Value: {}, Priority: {}", key, value, priority)
        });
    }

    #[test]
    fn remove_twice() {
        let mut treap = Treap::new();

        // Insert some values with custom priorities.
        //
        treap.insert(5, "five", 50);
        treap.insert(2, "two", 20);
        treap.insert(7, "seven", 70);

        assert_eq!(Some("five"), treap.remove(&5));
        assert_eq!(None, treap.remove(&5));
    }

    #[test]
    fn verify_contains() {
        let mut treap = Treap::new();

        // Insert some values with custom priorities.
        //
        treap.insert(1, "one", 1);
        treap.insert(2, "two", 2);
        treap.insert(3, "three", 3);

        assert_eq!(true, treap.contains(&2));
        assert_eq!(false, treap.contains(&5));
    }

    #[test]
    fn verify_gets() {
        let mut treap = Treap::new();

        // Insert some values with custom priorities.
        //
        treap.insert(100, "100", 100);
        treap.insert(200, "200", 200);
        treap.insert(300, "300", 300);

        assert_eq!(Some(&"200"), treap.get(&200));
    }
}
