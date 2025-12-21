// Marked pointer operations using two LSBs as mark bits.
//
// Bit layout:
//   Bit 0: DELETE_MARK - node is being deleted (logically removed from list)
//   Bit 1: UPDATE_MARK - node is being updated (follow prev/backlink to new node)
//
// Mark combinations:
//   0b00 (0): Normal, unmarked node
//   0b01 (1): DELETE-marked - node is being deleted
//   0b10 (2): UPDATE-marked - node has been updated, follow backlink for new value
//   0b11 (3): Invalid (update should complete before delete can happen)
//
const DELETE_MARK: usize = 0b01;
const UPDATE_MARK: usize = 0b10;
const ALL_MARKS: usize = 0b11;

/// A pointer that uses the two least significant bits as mark flags.
#[derive(Copy, Clone)]
pub(crate) struct MarkedPtr<T> {
    ptr: *mut T,
}

impl<T> MarkedPtr<T> {
    // =========================================================================
    // Construction
    // =========================================================================

    /// Create a new MarkedPtr from a (possibly marked) pointer.
    #[inline]
    pub(crate) fn new(ptr: *mut T) -> Self {
        MarkedPtr { ptr }
    }

    /// Strip mark bits from a raw pointer without creating a MarkedPtr instance.
    #[inline]
    pub(crate) fn unmask(ptr: *mut T) -> *mut T {
        (ptr as usize & !ALL_MARKS) as *mut T
    }

    // =========================================================================
    // Extraction
    // =========================================================================

    /// Get the clean pointer without mark bits (the one you dereference).
    #[inline]
    pub(crate) fn as_ptr(&self) -> *mut T {
        (self.ptr as usize & !ALL_MARKS) as *mut T
    }

    /// Get the raw pointer with mark bits intact (for CAS operations).
    #[inline]
    pub(crate) fn as_raw(&self) -> *mut T {
        self.ptr
    }

    // =========================================================================
    // Predicates
    // =========================================================================

    /// Check if DELETE-marked (bit 0).
    #[inline]
    pub(crate) fn is_marked(&self) -> bool {
        (self.ptr as usize & DELETE_MARK) != 0
    }

    /// Check if UPDATE-marked (bit 1).
    #[inline]
    pub(crate) fn is_update_marked(&self) -> bool {
        (self.ptr as usize & UPDATE_MARK) != 0
    }

    /// Check if any mark bit is set.
    #[inline]
    pub(crate) fn is_any_marked(&self) -> bool {
        (self.ptr as usize & ALL_MARKS) != 0
    }

    // =========================================================================
    // Transformers
    // =========================================================================

    /// Create DELETE-marked version of this pointer.
    #[inline]
    pub(crate) fn with_mark(&self, mark: bool) -> Self {
        let ptr_bits = self.as_ptr() as usize;
        let current_update = self.ptr as usize & UPDATE_MARK;
        let marked_bits = if mark {
            ptr_bits | DELETE_MARK | current_update
        } else {
            ptr_bits | current_update
        };
        MarkedPtr {
            ptr: marked_bits as *mut T,
        }
    }

    /// Create UPDATE-marked version of this pointer.
    #[inline]
    pub(crate) fn with_update_mark(&self, mark: bool) -> Self {
        let ptr_bits = self.as_ptr() as usize;
        let current_delete = self.ptr as usize & DELETE_MARK;
        let marked_bits = if mark {
            ptr_bits | UPDATE_MARK | current_delete
        } else {
            ptr_bits | current_delete
        };
        MarkedPtr {
            ptr: marked_bits as *mut T,
        }
    }
}
