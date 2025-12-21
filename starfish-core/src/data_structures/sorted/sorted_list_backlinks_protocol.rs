//! # Lock-Free Sorted List with Safe Backlinks
//!
//! Design document for a lock-free sorted list that supports safe backlink
//! traversal for epoch-based memory reclamation.
//!
//! ## Problem
//!
//! In lock-free linked lists with epoch-based reclamation:
//! - Forward traversal is safe (nodes protected by epoch guards)
//! - Backward traversal via `prev` pointers is UNSAFE (predecessor could be freed)
//!
//! ## Solution: DEL_NEXT Protocol
//!
//! Use three mark bits in the `next` pointer:
//!
//! ```text
//! Bit 0: DELETE   - Node is logically deleted
//! Bit 1: UPDATE   - Node superseded by new version (follow next)
//! Bit 2: DEL_NEXT - "I am about to delete my successor"
//! ```
//!
//! Key insight: DEL_NEXT creates a happens-before relationship that
//! guarantees the predecessor is alive when we follow a backlink.
//!
//! ## DELETE Protocol (4 Steps)
//!
//! To delete `curr` with predecessor `pred`:
//!
//! ```text
//! Initial: pred → curr → succ
//!
//! Step 1: Claim ownership (blocks inserts between pred and curr)
//!     CAS pred.next: curr → curr|DEL_NEXT
//!
//! Step 2: Set backlink (safe - we own curr)
//!     Store curr.prev = pred
//!
//! Step 3: Mark as deleted
//!     CAS curr.next: succ → succ|DELETE
//!
//! Step 4: Unlink
//!     CAS pred.next: curr|DEL_NEXT → succ
//! ```
//!
//! ## Safety Guarantee
//!
//! When thread sees DELETE on curr.next:
//! 1. Step 2 happened-before Step 3 (same thread)
//! 2. Therefore curr.prev is valid
//! 3. pred set DEL_NEXT before DELETE was set
//! 4. Therefore pred is still alive (epoch protection)
//!
//! ## Helping Protocol
//!
//! Any thread seeing `pred.next = curr|DEL_NEXT` must help:
//!
//! ```text
//! help_delete(pred, curr):
//!     // Help Step 3
//!     loop:
//!         if curr.next has DELETE: break
//!         if curr.next has UPDATE: return  // Don't interfere
//!         CAS curr.next: X → X|DELETE
//!
//!     // Help Step 4
//!     CAS pred.next: curr|DEL_NEXT → curr.next.as_ptr()
//! ```
//!
//! ## Insert Protocol
//!
//! ```text
//! insert(key):
//!     new_node = allocate(key)
//!     loop:
//!         (pred, curr) = find(key)
//!         if curr.key == key: return false
//!
//!         // Must help if DEL_NEXT is set
//!         if pred.next has DEL_NEXT:
//!             help_delete(pred, pred.next.as_ptr())
//!             continue
//!
//!         new_node.next = curr
//!         if CAS pred.next: curr → new_node:
//!             return true
//! ```
//!
//! ## Traversal with Snipping
//!
//! ```text
//! traverse(key):
//!     pred = head
//!     curr = pred.next.as_ptr()
//!
//!     loop:
//!         if curr is null or curr.key >= key:
//!             return (pred, curr)
//!
//!         // Help DEL_NEXT deletions
//!         if pred.next has DEL_NEXT:
//!             help_delete(pred, curr)
//!             curr = pred.next.as_ptr()
//!             continue
//!
//!         // Snip DELETE/UPDATE marked nodes
//!         if curr.next has DELETE or UPDATE:
//!             CAS pred.next: curr → curr.next.as_ptr()
//!             curr = pred.next.as_ptr()
//!             continue
//!
//!         pred = curr
//!         curr = curr.next.as_ptr()
//! ```
//!
//! ## Backlink Recovery
//!
//! When predecessor is deleted during our operation:
//!
//! ```text
//! find_valid_pred(node):
//!     curr = node
//!     loop:
//!         if curr is sentinel: return curr
//!         if curr.next has no DELETE/UPDATE: return curr
//!         curr = curr.prev  // Safe! DELETE guarantees valid backlink
//! ```
//!
//! ## Concurrent Scenario: Adjacent Deletions
//!
//! ```text
//! Initial: A → B → C → D
//! T1 deletes B, T2 deletes C
//!
//! T1: CAS A.next: B → B|DEL_NEXT           ✓
//! T2: CAS B.next: C → C|DEL_NEXT           fails (B has DEL_NEXT)
//! T2: helps T1 complete
//! T2: retries from A, deletes C
//! ```
//!
//! ## Concurrent Scenario: Predecessor Deleted
//!
//! ```text
//! Initial: A → B → C → D
//! T1 deletes C (pred=B), T2 deletes B
//!
//! T1: CAS B.next: C → C|DEL_NEXT           ✓
//! T1: Store C.prev = B
//! T1: CAS C.next: D → D|DELETE             ✓
//! T2: deletes B, unlinks it: A → C
//! T1: CAS B.next: C|DEL_NEXT → D           fails (B has DELETE)
//! T1: uses backlink: C.prev=B, B.prev=A
//! T1: finds A is valid, pred=A
//! T1: CAS A.next: C → C|DEL_NEXT           ✓
//! T1: CAS A.next: C|DEL_NEXT → D           ✓
//! ```
//!
//! ## Invariants
//!
//! 1. DEL_NEXT ownership: Only one thread sets DEL_NEXT for a given successor
//! 2. Backlink validity: DELETE on curr.next implies curr.prev is valid
//! 3. No lost nodes: Node freed only after physical unlinking
//! 4. Progress: Helping ensures operations complete
//! 5. Linearizable: Insert at CAS, Delete at DELETE mark, Find at key read
