//! A shared, writable file mapping (`mmap(MAP_SHARED)`), with `msync`.
//!
//! Under the simulation the mapping points straight at the simulated page cache,
//! so plain stores through it dirty the file; only `flush` (`msync(MS_SYNC)`)
//! makes them durable, and power loss before it may keep any subset of the
//! dirty pages (msync(2)).

use std::io;
use std::ops::{Deref, DerefMut};

use crate::fs::File;

enum Repr {
    Real { ptr: *mut u8, len: usize },
    #[cfg(feature = "sim")]
    Sim { id: u64, ptr: *mut u8, len: usize, pid: hegel_sim_core::Pid },
}

/// A shared, writable mapping of `len` bytes of a file from offset 0.
pub struct MmapMut(Repr);

// SAFETY: the mapping is plain shared memory, like `memmap2::MmapMut`; under the
// simulation every simulated thread runs on one OS thread.
unsafe impl Send for MmapMut {}
// SAFETY: as above; `&MmapMut` only reads.
unsafe impl Sync for MmapMut {}

fn real_map(fd: std::os::fd::RawFd, len: usize) -> io::Result<MmapMut> {
    // SAFETY: mmap of a descriptor we own; the result is checked.
    let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0) };
    if p == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(MmapMut(Repr::Real { ptr: p.cast(), len }))
}

impl MmapMut {
    /// Map the first `len` bytes of `file` shared and writable. The file must be
    /// at least `len` bytes long to touch every byte (mmap(2): SIGBUS past EOF).
    pub fn map_mut(file: &File, len: usize) -> io::Result<MmapMut> {
        #[cfg(not(feature = "sim"))]
        return real_map(std::os::fd::AsRawFd::as_raw_fd(file), len);
        #[cfg(feature = "sim")]
        {
            if let Some(f) = file.real() {
                return real_map(std::os::fd::AsRawFd::as_raw_fd(f), len);
            }
            let fd = file.sim_fd().unwrap_or_else(|| unreachable!());
            use hegel_sim_core::kernel::SysOk;
            use hegel_sim_core::{SysResult, Syscall};
            match crate::rt::syscall(Syscall::Mmap { fd, offset: 0, len, shared: true }) {
                SysResult::Ok(SysOk::Map { id, addr, len }) => {
                    Ok(MmapMut(Repr::Sim { id, ptr: addr as *mut u8, len, pid: crate::rt::current_pid() }))
                }
                SysResult::Err(e) => Err(crate::errno::err(e)),
                r => unreachable!("mmap returned {r:?}"),
            }
        }
    }

    fn parts(&self) -> (*mut u8, usize) {
        match self.0 {
            Repr::Real { ptr, len } => (ptr, len),
            #[cfg(feature = "sim")]
            Repr::Sim { ptr, len, .. } => (ptr, len),
        }
    }

    /// `msync(MS_SYNC)` of the whole mapping.
    pub fn flush(&self) -> io::Result<()> {
        let (_, len) = self.parts();
        self.flush_range(0, len)
    }

    /// `msync(MS_SYNC)` of `[offset, offset+len)`.
    pub fn flush_range(&self, offset: usize, len: usize) -> io::Result<()> {
        match self.0 {
            Repr::Real { ptr, .. } => {
                let page = 4096usize.max(page_size());
                let start = offset / page * page;
                // SAFETY: the range lies inside our mapping, page aligned.
                if unsafe { libc::msync(ptr.add(start).cast(), len + (offset - start), libc::MS_SYNC) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            }
            #[cfg(feature = "sim")]
            Repr::Sim { id, .. } => {
                use hegel_sim_core::{SysResult, Syscall};
                match crate::rt::syscall(Syscall::Msync { map: id, offset: offset as u64, len: len as u64 }) {
                    SysResult::Ok(_) => Ok(()),
                    SysResult::Err(e) => Err(crate::errno::err(e)),
                    r => unreachable!("msync returned {r:?}"),
                }
            }
        }
    }
}

fn page_size() -> usize {
    // SAFETY: sysconf takes a constant.
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

impl Deref for MmapMut {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        let (p, l) = self.parts();
        // SAFETY: the mapping is live for `l` bytes until drop.
        unsafe { std::slice::from_raw_parts(p, l) }
    }
}

impl DerefMut for MmapMut {
    fn deref_mut(&mut self) -> &mut [u8] {
        let (p, l) = self.parts();
        // SAFETY: as in `deref`, and `&mut self` is exclusive.
        unsafe { std::slice::from_raw_parts_mut(p, l) }
    }
}

impl Drop for MmapMut {
    fn drop(&mut self) {
        match self.0 {
            Repr::Real { ptr, len } => {
                // SAFETY: our own mapping.
                unsafe { libc::munmap(ptr.cast(), len) };
            }
            #[cfg(feature = "sim")]
            Repr::Sim { id, pid, .. } => {
                if crate::rt::in_sim() && crate::rt::current_pid() == pid {
                    let _ = crate::rt::syscall(hegel_sim_core::Syscall::Munmap { map: id });
                }
            }
        }
    }
}
