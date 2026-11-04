//! A file's page-cache bytes: a plain vector until something maps it, then a
//! pinned allocation that never moves, so mapped pointers stay valid as the file
//! grows within its reserve.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ptr::NonNull;

use super::{Errno, PAGE};

pub(crate) struct Pinned {
    ptr: NonNull<u8>,
    cap: usize,
    len: usize,
}

// SAFETY: `Pinned` owns its allocation exclusively, like a `Vec<u8>`; mapped
// pointers into it are only dereferenced by simulated threads, which run on the
// same OS thread as the kernel and never concurrently with it.
unsafe impl Send for Pinned {}

impl Drop for Pinned {
    fn drop(&mut self) {
        // SAFETY: allocated in `Buf::pin` with exactly this layout.
        unsafe { dealloc(self.ptr.as_ptr(), Layout::from_size_align_unchecked(self.cap, PAGE as usize)) }
    }
}

pub(crate) enum Buf {
    Heap(Vec<u8>),
    Pinned(Pinned),
}

impl std::fmt::Debug for Buf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Buf(len={}, pinned={})", self.len(), self.is_pinned())
    }
}

impl Buf {
    pub(crate) fn from_vec(v: Vec<u8>) -> Buf {
        Buf::Heap(v)
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Buf::Heap(v) => v.len(),
            Buf::Pinned(p) => p.len,
        }
    }

    pub(crate) fn is_pinned(&self) -> bool {
        matches!(self, Buf::Pinned(_))
    }

    /// The bytes. Reads through the base pointer, so stores made through mapped
    /// pointers are seen.
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Buf::Heap(v) => v,
            // SAFETY: `len <= cap`, the allocation is live and initialized (zeroed
            // at allocation), and no simulated thread runs while the kernel reads.
            Buf::Pinned(p) => unsafe { std::slice::from_raw_parts(p.ptr.as_ptr(), p.len) },
        }
    }

    pub(crate) fn to_vec(&self) -> Vec<u8> {
        self.bytes().to_vec()
    }

    /// Resize to `new_len`, zero-filling growth (ftruncate(2): "the extended part
    /// reads as null bytes").
    pub(crate) fn set_len(&mut self, new_len: usize) -> Result<(), Errno> {
        match self {
            Buf::Heap(v) => {
                v.resize(new_len, 0);
                Ok(())
            }
            Buf::Pinned(p) => {
                if new_len > p.cap {
                    return Err(Errno::EFBIG);
                }
                if new_len > p.len {
                    // SAFETY: `[len, new_len)` is inside the allocation.
                    unsafe { std::ptr::write_bytes(p.ptr.as_ptr().add(p.len), 0, new_len - p.len) };
                }
                p.len = new_len;
                Ok(())
            }
        }
    }

    pub(crate) fn write_at(&mut self, offset: usize, data: &[u8]) -> Result<(), Errno> {
        let end = offset + data.len();
        if end > self.len() {
            self.set_len(end)?;
        }
        match self {
            Buf::Heap(v) => v[offset..end].copy_from_slice(data),
            // SAFETY: `end <= len <= cap` after `set_len`.
            Buf::Pinned(p) => unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), p.ptr.as_ptr().add(offset), data.len())
            },
        }
        Ok(())
    }

    /// Pin the bytes in an allocation of at least `reserve` bytes and return its
    /// base pointer. Already pinned: the existing base.
    pub(crate) fn pin(&mut self, reserve: usize) -> Result<NonNull<u8>, Errno> {
        if let Buf::Pinned(p) = self {
            return Ok(p.ptr);
        }
        let len = self.len();
        let cap = reserve.max(len).div_ceil(PAGE as usize) * PAGE as usize;
        let cap = cap.max(PAGE as usize);
        let layout = Layout::from_size_align(cap, PAGE as usize).map_err(|_| Errno::ENOMEM)?;
        // SAFETY: `layout` has nonzero size.
        let raw = unsafe { alloc_zeroed(layout) };
        let ptr = NonNull::new(raw).ok_or(Errno::ENOMEM)?;
        // SAFETY: `len <= cap` and the source is a distinct allocation.
        unsafe { std::ptr::copy_nonoverlapping(self.bytes().as_ptr(), ptr.as_ptr(), len) };
        *self = Buf::Pinned(Pinned { ptr, cap, len });
        Ok(ptr)
    }
}
