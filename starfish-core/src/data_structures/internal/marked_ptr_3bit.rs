//! 3-bit marked pointer for backlinks delete protocol.
//!
//! Bit layout:
//!   Bit 0: DELETE   - Node is logically deleted
//!   Bit 1: UPDATE   - Node is being updated (follow next to new node)
//!   Bit 2: DEL_NEXT - Predecessor is about to unlink its successor
//!
//! Delete Protocol Order:
//! 1. Set DEL_NEXT on pred.next (blocks inserts between pred and curr)
//! 2. Set DELETE on curr.next (logical deletion)
//! 3. CAS pred.next to curr.next (physical unlink)

const DELETE_MARK: usize = 0b001;
const UPDATE_MARK: usize = 0b010;
const DEL_NEXT_MARK: usize = 0b100;
const ALL_MARKS: usize = 0b111;

/// A pointer that uses three least significant bits as mark flags.
#[derive(Copy, Clone, Debug)]
pub(crate) struct MarkedPtr3Bit<T> {
    ptr: *mut T,
}

impl<T> MarkedPtr3Bit<T> {
    // =========================================================================
    // Construction
    // =========================================================================

    /// Create a new MarkedPtr3Bit from a (possibly marked) pointer.
    #[inline]
    pub(crate) fn new(ptr: *mut T) -> Self {
        MarkedPtr3Bit { ptr }
    }

    /// Strip all mark bits from a raw pointer.
    #[inline]
    pub(crate) fn unmask(ptr: *mut T) -> *mut T {
        (ptr as usize & !ALL_MARKS) as *mut T
    }

    // =========================================================================
    // Extraction
    // =========================================================================

    /// Get the clean pointer without mark bits (safe to dereference).
    #[inline]
    pub(crate) fn as_ptr(&self) -> *mut T {
        (self.ptr as usize & !ALL_MARKS) as *mut T
    }

    /// Get the raw pointer with mark bits intact (for CAS operations).
    #[inline]
    pub(crate) fn as_raw(&self) -> *mut T {
        self.ptr
    }

    /// Get all mark bits.
    #[inline]
    pub(crate) fn marks(&self) -> usize {
        self.ptr as usize & ALL_MARKS
    }

    // =========================================================================
    // Predicates
    // =========================================================================

    /// Check if DELETE-marked (bit 0) - node is logically deleted.
    #[inline]
    pub(crate) fn is_deleted(&self) -> bool {
        (self.ptr as usize & DELETE_MARK) != 0
    }

    /// Check if UPDATE-marked (bit 1) - node is being updated.
    #[inline]
    pub(crate) fn is_update_marked(&self) -> bool {
        (self.ptr as usize & UPDATE_MARK) != 0
    }

    /// Check if DEL_NEXT-marked (bit 2) - about to unlink successor.
    #[inline]
    pub(crate) fn is_del_next(&self) -> bool {
        (self.ptr as usize & DEL_NEXT_MARK) != 0
    }

    /// Check if any mark bit is set.
    #[inline]
    pub(crate) fn is_any_marked(&self) -> bool {
        (self.ptr as usize & ALL_MARKS) != 0
    }

    /// Check if node should be skipped during traversal (DELETE or UPDATE).
    #[inline]
    pub(crate) fn should_skip(&self) -> bool {
        (self.ptr as usize & (DELETE_MARK | UPDATE_MARK)) != 0
    }

    // =========================================================================
    // Transformers
    // =========================================================================

    /// Create DELETE-marked version of this pointer (preserves other marks).
    #[inline]
    pub(crate) fn with_delete(&self) -> Self {
        MarkedPtr3Bit {
            ptr: (self.ptr as usize | DELETE_MARK) as *mut T,
        }
    }

    /// Create UPDATE-marked version of this pointer (preserves other marks).
    #[inline]
    pub(crate) fn with_update(&self) -> Self {
        MarkedPtr3Bit {
            ptr: (self.ptr as usize | UPDATE_MARK) as *mut T,
        }
    }

    /// Create DEL_NEXT-marked version of this pointer (preserves other marks).
    #[inline]
    pub(crate) fn with_del_next(&self) -> Self {
        MarkedPtr3Bit {
            ptr: (self.ptr as usize | DEL_NEXT_MARK) as *mut T,
        }
    }

    /// Clear DEL_NEXT mark (preserves other marks).
    #[inline]
    pub(crate) fn without_del_next(&self) -> Self {
        MarkedPtr3Bit {
            ptr: (self.ptr as usize & !DEL_NEXT_MARK) as *mut T,
        }
    }

    /// Clear all marks.
    #[inline]
    pub(crate) fn unmarked(&self) -> Self {
        MarkedPtr3Bit { ptr: self.as_ptr() }
    }

    /// Create pointer with specific marks.
    #[inline]
    pub(crate) fn with_marks(&self, marks: usize) -> Self {
        MarkedPtr3Bit {
            ptr: (self.as_ptr() as usize | (marks & ALL_MARKS)) as *mut T,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_marking_operations() {
        let ptr = Box::into_raw(Box::new(42i32));

        let marked = MarkedPtr3Bit::new(ptr);
        assert!(!marked.is_deleted());
        assert!(!marked.is_update_marked());
        assert!(!marked.is_del_next());
        assert!(!marked.is_any_marked());

        let deleted = marked.with_delete();
        assert!(deleted.is_deleted());
        assert!(!deleted.is_update_marked());
        assert!(!deleted.is_del_next());
        assert!(deleted.is_any_marked());
        assert_eq!(deleted.as_ptr(), ptr);

        let del_next = marked.with_del_next();
        assert!(!del_next.is_deleted());
        assert!(del_next.is_del_next());
        assert!(del_next.is_any_marked());

        let both = del_next.with_delete();
        assert!(both.is_deleted());
        assert!(both.is_del_next());
        assert_eq!(both.as_ptr(), ptr);

        let cleared = both.without_del_next();
        assert!(cleared.is_deleted());
        assert!(!cleared.is_del_next());

        unsafe {
            drop(Box::from_raw(ptr));
        }
    }

    #[test]
    fn test_unmask() {
        let ptr = Box::into_raw(Box::new(42i32));

        // Mark with all bits
        let marked = (ptr as usize | ALL_MARKS) as *mut i32;
        let clean = MarkedPtr3Bit::unmask(marked);
        assert_eq!(clean, ptr);

        unsafe {
            drop(Box::from_raw(ptr));
        }
    }
}
