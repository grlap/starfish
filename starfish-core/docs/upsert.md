# Upsert for Sorted Collections

**Status:** NOT IMPLEMENTED
**Crate:** starfish-core
**Key files:** src/data_structures/sorted/sorted_list.rs, src/data_structures/sorted/skip_list.rs

## Goal

Add `upsert(key: T)` — ensures the element is in the collection after the call.
If the key doesn't exist, insert it. If it already exists, update (replace) it.
The return value doesn't matter; the postcondition is what counts.

## Current API

| Method   | Key exists       | Key missing      |
|----------|------------------|------------------|
| `insert` | returns `None`   | inserts, returns position |
| `update` | replaces, returns old node | returns `None` |
| **`upsert`** | **replaces** | **inserts** |

## Algorithm (SortedList)

Both `insert` and `update` share the same search via `node_location_from_internal(key)`,
which returns `(pred, curr)` where `curr.key >= key`. The upsert loop branches on
whether `curr.key == key`.

```
allocate new_node(key)

loop {
    (pred, curr) = node_location_from_internal(key)

    if curr is not null AND curr.key == key {
        // --- UPDATE path ---
        curr_next = curr.get_next()

        if curr_next is DELETE-marked:
            continue  // node is being deleted, retry (will insert or find replacement)

        if curr_next is UPDATE-marked:
            continue  // another update in progress, retry

        new_node.next = curr_next.ptr   // point past curr's successor
        CAS curr.next: curr_next -> (new_node | UPDATE_MARK)
            fail -> continue

        // Linearized: curr is now UPDATE-marked, new_node is live
        unlink curr from pred
        defer_destroy(curr)
        return new_node position

    } else {
        // --- INSERT path ---
        new_node.next = curr              // may be null (end of list)
        CAS pred.next: curr -> new_node
            fail -> continue

        return new_node position
    }
}
```

### Why this works under concurrency

The loop naturally handles all races:

| Race condition | What happens |
|---|---|
| Concurrent delete removes `curr` between search and CAS | UPDATE-path CAS fails (curr.next changed), retry. Next iteration either inserts (key gone) or finds replacement. |
| Concurrent insert adds duplicate between search and CAS | INSERT-path CAS fails (pred.next changed), retry. Next iteration finds the key and takes UPDATE path. |
| Concurrent update replaces `curr` | UPDATE-path sees UPDATE_MARK on curr.next, retries. Next iteration finds the new node. |
| `pred` gets marked between search and CAS | CAS fails (pred.next is marked), retry. `node_location_from_internal` cleans up marked nodes on next traversal. |

The key insight: **we never need to "switch" from insert to update or vice versa within a single CAS attempt.** We observe the state, pick the path, try one CAS. If it fails, the loop re-searches and picks the correct path for the new state.

### Node allocation

One `new_node` is allocated before the loop and reused across retries. Both paths use the same node — only the linking differs. This is the same pattern `insert_from_internal` and `update_internal` already use individually.

## Trait-Level API

Add to `SortedCollection<T>`:

```rust
/// Internal upsert: insert or update.
fn upsert_from_internal(
    &self,
    key: T,
    position: Option<&Self::NodePosition>,
) -> Self::NodePosition;
```

Note: returns `NodePosition` (not `Option`) — upsert always succeeds.

Safe public method (default impl on the trait):

```rust
fn upsert(&self, key: T) {
    let _guard = Self::Guard::pin();
    let pos = self.upsert_from_internal(key, None);
    // No old node to destroy on insert path;
    // update path handles defer_destroy internally.
}
```

Actually — the update path produces an old node that needs deferred destruction.
So the internal method should signal which path was taken:

```rust
fn upsert_from_internal(
    &self,
    key: T,
    position: Option<&Self::NodePosition>,
) -> UpsertResult<Self::Node, Self::NodePosition>;

enum UpsertResult<N, P> {
    Inserted(P),
    Updated { old_node: *mut N, position: P },
}
```

The trait default:

```rust
fn upsert(&self, key: T) {
    let _guard = Self::Guard::pin();
    match self.upsert_from_internal(key, None) {
        UpsertResult::Inserted(_) => {}
        UpsertResult::Updated { old_node, .. } => unsafe {
            self.guard().defer_destroy(old_node, Self::Node::dealloc_ptr);
        },
    }
}
```

## Implementation order

1. **SortedList** — simplest, single-level linked list, easiest to verify
2. **SkipList** — map (`MapEntry`): try insert, else value-CAS the value (no node unlinking); set: insert-or-noop
3. **SkipTrie** — if needed

## Testing

- Basic: upsert into empty, upsert existing key, upsert new key
- Idempotency: upsert same key N times, collection has exactly 1 element
- Concurrent: N threads upsert overlapping key sets, verify no duplicates and all keys present
- Mixed: concurrent insert + delete + upsert on same keys
