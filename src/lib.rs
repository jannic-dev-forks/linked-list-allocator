#![cfg_attr(feature = "const_mut_refs", feature(const_mut_refs))]
#![cfg_attr(
    feature = "alloc_ref",
    feature(allocator_api, alloc_layout_extra, nonnull_slice_from_raw_parts)
)]
#![no_std]

#[cfg(test)]
#[macro_use]
extern crate std;

#[cfg(feature = "use_spin")]
extern crate spinning_top;

#[cfg(feature = "use_spin")]
use core::alloc::GlobalAlloc;
use core::alloc::Layout;
#[cfg(feature = "alloc_ref")]
use core::alloc::{AllocError, Allocator};
use core::mem::MaybeUninit;
#[cfg(feature = "use_spin")]
use core::ops::Deref;
use core::ptr::NonNull;
#[cfg(test)]
use hole::Hole;
use hole::HoleList;
#[cfg(feature = "use_spin")]
use spinning_top::Spinlock;

pub mod hole;
#[cfg(test)]
mod test;

/// A fixed size heap backed by a linked list of free memory blocks.
pub struct Heap {
    used: usize,
    holes: HoleList,
}

unsafe impl Send for Heap {}

impl Heap {
    /// Creates an empty heap. All allocate calls will return `None`.
    #[cfg(not(feature = "const_mut_refs"))]
    pub fn empty() -> Heap {
        Heap {
            used: 0,
            holes: HoleList::empty(),
        }
    }

    #[cfg(feature = "const_mut_refs")]
    pub const fn empty() -> Heap {
        Heap {
            used: 0,
            holes: HoleList::empty(),
        }
    }

    /// Initializes an empty heap
    ///
    /// # Safety
    ///
    /// This function must be called at most once and must only be used on an
    /// empty heap.
    ///
    /// The bottom address must be valid and the memory in the
    /// `[heap_bottom, heap_bottom + heap_size)` range must not be used for anything else.
    /// This function is unsafe because it can cause undefined behavior if the given address
    /// is invalid.
    ///
    /// The provided memory range must be valid for the `'static` lifetime.
    pub unsafe fn init(&mut self, heap_bottom: *mut u8, heap_size: usize) {
        self.used = 0;
        self.holes = HoleList::new(heap_bottom, heap_size);
    }

    /// Initialize an empty heap with provided memory.
    ///
    /// The caller is responsible for procuring a region of raw memory that may be utilized by the
    /// allocator. This might be done via any method such as (unsafely) taking a region from the
    /// program's memory, from a mutable static, or by allocating and leaking such memory from
    /// another allocator.
    ///
    /// The latter method may be especially useful if the underlying allocator does not perform
    /// deallocation (e.g. a simple bump allocator). Then the overlaid linked-list-allocator can
    /// provide memory reclamation.
    ///
    /// # Panics
    ///
    /// This method panics if the heap is already initialized.
    pub fn init_from_slice(&mut self, mem: &'static mut [MaybeUninit<u8>]) {
        assert!(
            self.bottom().is_null(),
            "The heap has already been initialized."
        );
        let size = mem.len();
        let address = mem.as_mut_ptr().cast();
        // SAFETY: All initialization requires the bottom address to be valid, which implies it
        // must not be 0. Initially the address is 0. The assertion above ensures that no
        // initialization had been called before.
        // The given address and size is valid according to the safety invariants of the mutable
        // reference handed to us by the caller.
        unsafe { self.init(address, size) }
    }

    /// Creates a new heap with the given `bottom` and `size`.
    ///
    /// # Safety
    ///
    /// The bottom address must be valid and the memory in the
    /// `[heap_bottom, heap_bottom + heap_size)` range must not be used for anything else.
    /// This function is unsafe because it can cause undefined behavior if the given address
    /// is invalid.
    ///
    /// The provided memory range must be valid for the `'static` lifetime.
    pub unsafe fn new(heap_bottom: *mut u8, heap_size: usize) -> Heap {
        Heap {
            used: 0,
            holes: HoleList::new(heap_bottom, heap_size),
        }
    }

    /// Creates a new heap from a slice of raw memory.
    ///
    /// This has the same effect as [`init_from_slice`] on an empty heap, but it is combined into a
    /// single operation that can not panic.
    pub fn from_slice(mem: &'static mut [MaybeUninit<u8>]) -> Heap {
        let size = mem.len();
        let address = mem.as_mut_ptr().cast();
        // SAFETY: The given address and size is valid according to the safety invariants of the
        // mutable reference handed to us by the caller.
        unsafe { Self::new(address, size) }
    }

    /// Allocates a chunk of the given size with the given alignment. Returns a pointer to the
    /// beginning of that chunk if it was successful. Else it returns `None`.
    /// This function scans the list of free memory blocks and uses the first block that is big
    /// enough. The runtime is in O(n) where n is the number of free blocks, but it should be
    /// reasonably fast for small allocations.
    //
    // NOTE: We could probably replace this with an `Option` instead of a `Result` in a later
    // release to remove this clippy warning
    #[allow(clippy::result_unit_err)]
    pub fn allocate_first_fit(&mut self, layout: Layout) -> Result<NonNull<u8>, ()> {
        match self.holes.allocate_first_fit(layout) {
            Ok((ptr, aligned_layout)) => {
                self.used += aligned_layout.size();
                Ok(ptr)
            }
            Err(err) => Err(err),
        }
    }

    /// Frees the given allocation. `ptr` must be a pointer returned
    /// by a call to the `allocate_first_fit` function with identical size and alignment.
    ///
    /// This function walks the list of free memory blocks and inserts the freed block at the
    /// correct place. If the freed block is adjacent to another free block, the blocks are merged
    /// again. This operation is in `O(n)` since the list needs to be sorted by address.
    ///
    /// # Safety
    ///
    /// `ptr` must be a pointer returned by a call to the [`allocate_first_fit`] function with
    /// identical layout. Undefined behavior may occur for invalid arguments.
    pub unsafe fn deallocate(&mut self, ptr: NonNull<u8>, layout: Layout) {
        self.used -= self.holes.deallocate(ptr, layout).size();
    }

    /// Returns the bottom address of the heap.
    pub fn bottom(&self) -> *mut u8 {
        self.holes.bottom
    }

    /// Returns the size of the heap.
    pub fn size(&self) -> usize {
        (self.top() as usize) - (self.bottom() as usize)
    }

    /// Return the top address of the heap
    pub fn top(&self) -> *mut u8 {
        self.holes.top
    }

    /// Returns the size of the used part of the heap
    pub fn used(&self) -> usize {
        self.used
    }

    /// Returns the size of the free part of the heap
    pub fn free(&self) -> usize {
        self.size() - self.used
    }

    /// Extends the size of the heap by creating a new hole at the end.
    ///
    /// Panics when the heap extension fails. Use [`try_extend`] to handle potential errors.
    ///
    /// # Safety
    ///
    /// The amount of data given in `by` MUST exist directly after the original
    /// range of data provided when constructing the [Heap]. The additional data
    /// must have the same lifetime of the original range of data.
    pub unsafe fn extend(&mut self, by: usize) {
        self.holes.try_extend(by).expect("heap extension failed");
    }

    /// Tries to extend the size of the heap by creating a new hole at the end.
    ///
    /// # Safety
    ///
    /// The amount of data given in `by` MUST exist directly after the original
    /// range of data provided when constructing the [Heap]. The additional data
    /// must have the same lifetime of the original range of data.
    pub unsafe fn try_extend(&mut self, by: usize) -> Result<(), ExtendError> {
        self.holes.try_extend(by)
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum ExtendError {
    /// The given extension size was not large enough to store the necessary metadata.
    SizeTooSmall,
    /// The given extension size was must be a multiple of 16.
    OddSize,
    /// The heap size must be a multiple of 16, otherwise extension is not possible.
    OddHeapSize,
    /// Extending an empty heap is not possible.
    EmptyHeap,
}

impl core::fmt::Display for ExtendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            ExtendError::SizeTooSmall => {
                "heap extension size was not large enough to store the necessary metadata"
            }
            ExtendError::OddSize => "heap extension size is not a multiple of 16",
            ExtendError::OddHeapSize => {
                "heap extension not possible because heap size is not a multiple of 16"
            }
            ExtendError::EmptyHeap => "tried to extend an emtpy heap",
        })
    }
}

#[cfg(all(feature = "alloc_ref", feature = "use_spin"))]
unsafe impl Allocator for LockedHeap {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            return Ok(NonNull::slice_from_raw_parts(layout.dangling(), 0));
        }
        match self.0.lock().allocate_first_fit(layout) {
            Ok(ptr) => Ok(NonNull::slice_from_raw_parts(ptr, layout.size())),
            Err(()) => Err(AllocError),
        }
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() != 0 {
            self.0.lock().deallocate(ptr, layout);
        }
    }
}

#[cfg(feature = "use_spin")]
pub struct LockedHeap(Spinlock<Heap>);

#[cfg(feature = "use_spin")]
impl LockedHeap {
    /// Creates an empty heap. All allocate calls will return `None`.
    #[cfg(feature = "use_spin_nightly")]
    pub const fn empty() -> LockedHeap {
        LockedHeap(Spinlock::new(Heap::empty()))
    }

    /// Creates an empty heap. All allocate calls will return `None`.
    #[cfg(not(feature = "use_spin_nightly"))]
    pub fn empty() -> LockedHeap {
        LockedHeap(Spinlock::new(Heap::empty()))
    }

    /// Creates a new heap with the given `bottom` and `size`.
    ///
    /// # Safety
    ///
    /// The bottom address must be valid and the memory in the
    /// `[heap_bottom, heap_bottom + heap_size)` range must not be used for anything else.
    /// This function is unsafe because it can cause undefined behavior if the given address
    /// is invalid.
    ///
    /// The provided memory range must be valid for the `'static` lifetime.
    pub unsafe fn new(heap_bottom: *mut u8, heap_size: usize) -> LockedHeap {
        LockedHeap(Spinlock::new(Heap {
            used: 0,
            holes: HoleList::new(heap_bottom, heap_size),
        }))
    }
}

#[cfg(feature = "use_spin")]
impl Deref for LockedHeap {
    type Target = Spinlock<Heap>;

    fn deref(&self) -> &Spinlock<Heap> {
        &self.0
    }
}

#[cfg(feature = "use_spin")]
unsafe impl GlobalAlloc for LockedHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        self.0
            .lock()
            .allocate_first_fit(layout)
            .ok()
            .map_or(core::ptr::null_mut(), |allocation| allocation.as_ptr())
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        self.0
            .lock()
            .deallocate(NonNull::new_unchecked(ptr), layout)
    }
}

/// Align downwards. Returns the greatest x with alignment `align`
/// so that x <= addr. The alignment must be a power of 2.
pub fn align_down_size(size: usize, align: usize) -> usize {
    if align.is_power_of_two() {
        size & !(align - 1)
    } else if align == 0 {
        size
    } else {
        panic!("`align` must be a power of 2");
    }
}

pub fn align_up_size(size: usize, align: usize) -> usize {
    align_down_size(size + align - 1, align)
}

/// Align upwards. Returns the smallest x with alignment `align`
/// so that x >= addr. The alignment must be a power of 2.
pub fn align_up(addr: *mut u8, align: usize) -> *mut u8 {
    let offset = addr.align_offset(align);
    addr.wrapping_add(offset)
}
