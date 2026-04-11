use std::alloc::{GlobalAlloc, Layout, System};
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;

/// An owned heap allocation backed by the **system** allocator.
///
/// Equivalent to `Box<T>` but always routes through [`System`] to avoid
/// bootstrap recursion when `T` is part of the NUMA allocator itself.
///
/// Like `Box`, `SysBox` is `!Copy` and `!Clone` — it enforces unique
/// ownership.  [`Drop`] runs `drop_in_place` on the contained value and
/// then frees the memory via `System.dealloc`.
pub(crate) struct SysBox<T> {
    ptr: NonNull<T>,
    /// Communicates ownership of `T` to the compiler (drop check, variance).
    _marker: PhantomData<T>,
}

impl<T> SysBox<T> {
    /// Allocate and move `val` into a new system-allocator–backed allocation.
    pub fn new(val: T) -> Self {
        let layout = Layout::new::<T>();
        // SAFETY: layout is non-zero size for all types used in this crate.
        let raw = unsafe { System.alloc(layout) } as *mut T;
        let Some(ptr) = NonNull::new(raw) else {
            std::alloc::handle_alloc_error(layout);
        };
        // SAFETY: `ptr` is valid, aligned, and exclusively owned.
        unsafe {
            ptr.as_ptr().write(val);
        }
        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Allocate zeroed memory via the system allocator.
    ///
    /// # Safety
    /// The caller must ensure that all-zeros is a valid bit pattern for `T`.
    pub unsafe fn new_zeroed() -> Self {
        let layout = Layout::new::<T>();
        // SAFETY: caller guarantees all-zeros is valid for T.
        let raw = unsafe { System.alloc_zeroed(layout) } as *mut T;
        let Some(ptr) = NonNull::new(raw) else {
            std::alloc::handle_alloc_error(layout);
        };
        Self {
            ptr,
            _marker: PhantomData,
        }
    }

    /// Return the inner [`NonNull`] pointer without consuming the `SysBox`.
    ///
    /// The caller must not free or drop the pointee — `SysBox` retains
    /// ownership and will free on drop.
    #[inline]
    pub fn as_non_null(&self) -> NonNull<T> {
        self.ptr
    }
}

impl<T> Deref for SysBox<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: `ptr` is valid, aligned, and exclusively owned.
        unsafe { self.ptr.as_ref() }
    }
}

impl<T> DerefMut for SysBox<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: `ptr` is valid, aligned, and exclusively owned.
        unsafe { self.ptr.as_mut() }
    }
}

impl<T> Drop for SysBox<T> {
    fn drop(&mut self) {
        unsafe {
            // SAFETY: `ptr` was initialised in `new`/`new_zeroed` and is
            // exclusively owned.  Drop the value, then free the memory.
            std::ptr::drop_in_place(self.ptr.as_ptr());
            System.dealloc(self.ptr.as_ptr() as *mut u8, Layout::new::<T>());
        }
    }
}

// SAFETY: `SysBox<T>` owns `T` exclusively, identical to `Box<T>`.
unsafe impl<T: Send> Send for SysBox<T> {}
unsafe impl<T: Sync> Sync for SysBox<T> {}
