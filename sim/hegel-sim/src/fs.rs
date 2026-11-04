//! `std::fs`, plus the durability and locking calls std lacks ([`FileExtras`],
//! [`sync_dir`]). Under the simulation every call is a syscall on the simulated
//! kernel (page cache, device cache, media; power loss tears what was not
//! synced); outside it, the real filesystem.
//!
//! Paths in the simulated filesystem are absolute; a relative path is taken from
//! `/`.

use std::io;
use std::path::Path;

#[cfg(not(feature = "sim"))]
pub use std::fs::*;

#[cfg(feature = "sim")]
pub use imp::*;
#[cfg(feature = "sim")]
pub use std::fs::{FileType, Permissions};

/// `fcntl(2)` record lock type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    /// `F_RDLCK`.
    Read,
    /// `F_WRLCK`.
    Write,
}

/// Durability and locking on a file, in both the real and the simulated build.
pub trait FileExtras {
    /// `fcntl(F_FULLFSYNC)` on macOS (flushes the drive's cache, which plain
    /// `fsync` there does not; fsync(2) on macOS); `fsync` elsewhere.
    fn full_fsync(&self) -> io::Result<()>;
    /// `flock(LOCK_EX)`, blocking.
    fn lock_exclusive(&self) -> io::Result<()>;
    /// `flock(LOCK_SH)`, blocking.
    fn lock_shared(&self) -> io::Result<()>;
    /// `flock(LOCK_EX | LOCK_NB)`: `Ok(false)` when another holder has it.
    fn try_lock_exclusive(&self) -> io::Result<bool>;
    /// `flock(LOCK_UN)`.
    fn unlock(&self) -> io::Result<()>;
    /// `fcntl(F_SETLK/F_SETLKW)` over `[start, start+len)` (`len == 0`: to EOF and
    /// beyond); `kind: None` unlocks. `Ok(false)`: not waiting and conflicting.
    fn fcntl_lock(&self, kind: Option<LockKind>, start: u64, len: u64, wait: bool) -> io::Result<bool>;
}

mod real {
    use std::io;
    use std::os::fd::RawFd;

    fn cvt(r: libc::c_int) -> io::Result<()> {
        if r == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    pub(super) fn full_fsync(fd: RawFd) -> io::Result<()> {
        #[cfg(target_vendor = "apple")]
        // SAFETY: fcntl on a descriptor we own.
        return cvt(unsafe { libc::fcntl(fd, libc::F_FULLFSYNC) });
        #[cfg(not(target_vendor = "apple"))]
        // SAFETY: fsync on a descriptor we own.
        return cvt(unsafe { libc::fsync(fd) });
    }

    pub(super) fn flock(fd: RawFd, op: libc::c_int) -> io::Result<bool> {
        // SAFETY: flock on a descriptor we own.
        match cvt(unsafe { libc::flock(fd, op) }) {
            Ok(()) => Ok(true),
            Err(e) if op & libc::LOCK_NB != 0 && e.raw_os_error() == Some(libc::EWOULDBLOCK) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub(super) fn fcntl_lock(fd: RawFd, kind: Option<super::LockKind>, start: u64, len: u64, wait: bool) -> io::Result<bool> {
        // SAFETY: a zeroed flock is valid; fields set below.
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = match kind {
            None => libc::F_UNLCK,
            Some(super::LockKind::Read) => libc::F_RDLCK,
            Some(super::LockKind::Write) => libc::F_WRLCK,
        } as _;
        fl.l_whence = libc::SEEK_SET as _;
        fl.l_start = start as _;
        fl.l_len = len as _;
        let cmd = if wait { libc::F_SETLKW } else { libc::F_SETLK };
        // SAFETY: fcntl with a valid flock pointer.
        match cvt(unsafe { libc::fcntl(fd, cmd, &fl) }) {
            Ok(()) => Ok(true),
            Err(e) if !wait && matches!(e.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EACCES)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub(super) fn sync_dir(path: &std::path::Path) -> io::Result<()> {
        std::fs::File::open(path)?.sync_all()
    }
}

impl FileExtras for std::fs::File {
    fn full_fsync(&self) -> io::Result<()> {
        real::full_fsync(std::os::fd::AsRawFd::as_raw_fd(self))
    }
    fn lock_exclusive(&self) -> io::Result<()> {
        real::flock(std::os::fd::AsRawFd::as_raw_fd(self), libc::LOCK_EX).map(drop)
    }
    fn lock_shared(&self) -> io::Result<()> {
        real::flock(std::os::fd::AsRawFd::as_raw_fd(self), libc::LOCK_SH).map(drop)
    }
    fn try_lock_exclusive(&self) -> io::Result<bool> {
        real::flock(std::os::fd::AsRawFd::as_raw_fd(self), libc::LOCK_EX | libc::LOCK_NB)
    }
    fn unlock(&self) -> io::Result<()> {
        real::flock(std::os::fd::AsRawFd::as_raw_fd(self), libc::LOCK_UN).map(drop)
    }
    fn fcntl_lock(&self, kind: Option<LockKind>, start: u64, len: u64, wait: bool) -> io::Result<bool> {
        real::fcntl_lock(std::os::fd::AsRawFd::as_raw_fd(self), kind, start, len, wait)
    }
}

/// fsync the directory at `path`, which makes a rename, create or unlink in it
/// durable (fsync(2): "Calling fsync() does not necessarily ensure that the
/// entry in the directory containing the file has also reached disk. For that
/// an explicit fsync() on a file descriptor for the directory is also needed.").
pub fn sync_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
    #[cfg(feature = "sim")]
    if crate::rt::in_sim() {
        return imp::sync_dir_sim(path.as_ref());
    }
    real::sync_dir(path.as_ref())
}

#[cfg(feature = "sim")]
mod imp {
    use std::ffi::OsString;
    use std::io::{self, Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};

    use hegel_sim_core::kernel::{Fd, FlockOp, OpenFlags, Stat, SysOk, Whence};
    use hegel_sim_core::{Pid, SysResult, Syscall};

    use super::{FileExtras, LockKind};
    use crate::errno::err;
    use crate::rt;

    fn sys(call: Syscall) -> io::Result<SysOk> {
        match rt::syscall(call) {
            SysResult::Ok(v) => Ok(v),
            SysResult::Err(e) => Err(err(e)),
            SysResult::Block(_) => unreachable!("hegel-sim: the driver retries blocked syscalls"),
        }
    }

    pub(super) fn sim_path(p: &Path) -> io::Result<String> {
        let s = p.to_str().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("hegel-sim: non-UTF-8 path {p:?}")))?;
        Ok(if s.starts_with('/') { s.to_owned() } else { format!("/{s}") })
    }

    fn unsupported(what: &str) -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, format!("hegel-sim: {what} is not modelled by the simulated kernel"))
    }

    #[derive(Debug)]
    enum Repr {
        Real(std::fs::File),
        Sim { fd: Fd, pid: Pid },
    }

    /// `std::fs::File`.
    #[derive(Debug)]
    pub struct File(Repr);

    impl File {
        /// See std.
        pub fn open<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new().read(true).open(path)
        }

        /// See std.
        pub fn create<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new().write(true).create(true).truncate(true).open(path)
        }

        /// See std.
        pub fn create_new<P: AsRef<Path>>(path: P) -> io::Result<File> {
            OpenOptions::new().read(true).write(true).create_new(true).open(path)
        }

        /// See std.
        pub fn options() -> OpenOptions {
            OpenOptions::new()
        }

        pub(crate) fn sim_fd(&self) -> Option<Fd> {
            match self.0 {
                Repr::Sim { fd, .. } => Some(fd),
                Repr::Real(_) => None,
            }
        }

        pub(crate) fn real(&self) -> Option<&std::fs::File> {
            match &self.0 {
                Repr::Real(f) => Some(f),
                Repr::Sim { .. } => None,
            }
        }

        /// See std.
        pub fn sync_all(&self) -> io::Result<()> {
            match &self.0 {
                Repr::Real(f) => f.sync_all(),
                Repr::Sim { fd, .. } => sys(Syscall::Fsync { fd: *fd }).map(drop),
            }
        }

        /// See std.
        pub fn sync_data(&self) -> io::Result<()> {
            match &self.0 {
                Repr::Real(f) => f.sync_data(),
                Repr::Sim { fd, .. } => sys(Syscall::Fdatasync { fd: *fd }).map(drop),
            }
        }

        /// See std.
        pub fn set_len(&self, size: u64) -> io::Result<()> {
            match &self.0 {
                Repr::Real(f) => f.set_len(size),
                Repr::Sim { fd, .. } => sys(Syscall::Ftruncate { fd: *fd, len: size }).map(drop),
            }
        }

        /// See std.
        pub fn metadata(&self) -> io::Result<Metadata> {
            match &self.0 {
                Repr::Real(f) => f.metadata().map(Metadata::Real),
                Repr::Sim { fd, .. } => match sys(Syscall::Fstat { fd: *fd })? {
                    SysOk::Stat(s) => Ok(Metadata::Sim(s)),
                    v => unreachable!("fstat returned {v:?}"),
                },
            }
        }

        /// See std. Not modelled under the simulation (`dup` is not in the kernel
        /// model): an `Unsupported` error.
        pub fn try_clone(&self) -> io::Result<File> {
            match &self.0 {
                Repr::Real(f) => f.try_clone().map(|f| File(Repr::Real(f))),
                Repr::Sim { .. } => Err(unsupported("File::try_clone (dup)")),
            }
        }

        fn read_sim(fd: Fd, buf: &mut [u8], offset: Option<u64>) -> io::Result<usize> {
            let call = match offset {
                None => Syscall::Read { fd, len: buf.len() },
                Some(offset) => Syscall::Pread { fd, len: buf.len(), offset },
            };
            match sys(call)? {
                SysOk::Bytes(b) => {
                    buf[..b.len()].copy_from_slice(&b);
                    Ok(b.len())
                }
                v => unreachable!("read returned {v:?}"),
            }
        }

        fn write_sim(fd: Fd, buf: &[u8], offset: Option<u64>) -> io::Result<usize> {
            let call = match offset {
                None => Syscall::Write { fd, data: buf.to_vec() },
                Some(offset) => Syscall::Pwrite { fd, data: buf.to_vec(), offset },
            };
            match sys(call)? {
                SysOk::Count(n) => Ok(n),
                v => unreachable!("write returned {v:?}"),
            }
        }

        fn flock(&self, op: FlockOp, nonblock: bool) -> io::Result<bool> {
            match &self.0 {
                Repr::Real(f) => {
                    let mut o = match op {
                        FlockOp::Shared => libc::LOCK_SH,
                        FlockOp::Exclusive => libc::LOCK_EX,
                        FlockOp::Unlock => libc::LOCK_UN,
                    };
                    if nonblock {
                        o |= libc::LOCK_NB;
                    }
                    super::real::flock(std::os::fd::AsRawFd::as_raw_fd(f), o)
                }
                Repr::Sim { fd, .. } => match sys(Syscall::Flock { fd: *fd, op, nonblock }) {
                    Ok(_) => Ok(true),
                    Err(e) if nonblock && e.raw_os_error() == Some(libc::EAGAIN) => Ok(false),
                    Err(e) => Err(e),
                },
            }
        }
    }

    impl Drop for File {
        fn drop(&mut self) {
            if let Repr::Sim { fd, pid } = self.0
                && rt::in_sim() && rt::current_pid() == pid {
                    let _ = rt::syscall(Syscall::Close { fd });
                }
        }
    }

    impl FileExtras for File {
        fn full_fsync(&self) -> io::Result<()> {
            match &self.0 {
                Repr::Real(f) => f.full_fsync(),
                Repr::Sim { fd, .. } => sys(Syscall::FullFsync { fd: *fd }).map(drop),
            }
        }
        fn lock_exclusive(&self) -> io::Result<()> {
            self.flock(FlockOp::Exclusive, false).map(drop)
        }
        fn lock_shared(&self) -> io::Result<()> {
            self.flock(FlockOp::Shared, false).map(drop)
        }
        fn try_lock_exclusive(&self) -> io::Result<bool> {
            self.flock(FlockOp::Exclusive, true)
        }
        fn unlock(&self) -> io::Result<()> {
            self.flock(FlockOp::Unlock, false).map(drop)
        }
        fn fcntl_lock(&self, kind: Option<LockKind>, start: u64, len: u64, wait: bool) -> io::Result<bool> {
            match &self.0 {
                Repr::Real(f) => f.fcntl_lock(kind, start, len, wait),
                Repr::Sim { fd, .. } => {
                    let kind = kind.map(|k| match k {
                        LockKind::Read => hegel_sim_core::kernel::LockKind::Read,
                        LockKind::Write => hegel_sim_core::kernel::LockKind::Write,
                    });
                    match sys(Syscall::FcntlLock { fd: *fd, kind, start, len, wait }) {
                        Ok(_) => Ok(true),
                        Err(e) if !wait && e.raw_os_error() == Some(libc::EAGAIN) => Ok(false),
                        Err(e) => Err(e),
                    }
                }
            }
        }
    }

    impl Read for &File {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match &self.0 {
                Repr::Real(f) => (&*f).read(buf),
                Repr::Sim { fd, .. } => File::read_sim(*fd, buf, None),
            }
        }
    }

    impl Write for &File {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            match &self.0 {
                Repr::Real(f) => (&*f).write(buf),
                Repr::Sim { fd, .. } => File::write_sim(*fd, buf, None),
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for &File {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            match &self.0 {
                Repr::Real(f) => (&*f).seek(pos),
                Repr::Sim { fd, .. } => {
                    let whence = match pos {
                        SeekFrom::Start(n) => Whence::Set(n),
                        SeekFrom::Current(n) => Whence::Cur(n),
                        SeekFrom::End(n) => Whence::End(n),
                    };
                    match sys(Syscall::Seek { fd: *fd, whence })? {
                        SysOk::Offset(o) => Ok(o),
                        v => unreachable!("lseek returned {v:?}"),
                    }
                }
            }
        }
    }

    impl Read for File {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            (&*self).read(buf)
        }
    }

    impl Write for File {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            (&*self).write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for File {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            (&*self).seek(pos)
        }
    }

    impl std::os::unix::fs::FileExt for File {
        fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
            match &self.0 {
                Repr::Real(f) => f.read_at(buf, offset),
                Repr::Sim { fd, .. } => File::read_sim(*fd, buf, Some(offset)),
            }
        }
        fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
            match &self.0 {
                Repr::Real(f) => f.write_at(buf, offset),
                Repr::Sim { fd, .. } => File::write_sim(*fd, buf, Some(offset)),
            }
        }
    }

    /// `std::fs::OpenOptions`. `custom_flags` understands `O_DSYNC` and `O_SYNC`
    /// under the simulation.
    #[derive(Clone, Debug, Default)]
    pub struct OpenOptions {
        flags: OpenFlags,
        create_new: bool,
        custom: i32,
        mode: u32,
    }

    impl OpenOptions {
        /// See std.
        pub fn new() -> OpenOptions {
            OpenOptions { mode: 0o666, ..OpenOptions::default() }
        }
        /// See std.
        pub fn read(&mut self, v: bool) -> &mut Self {
            self.flags.read = v;
            self
        }
        /// See std.
        pub fn write(&mut self, v: bool) -> &mut Self {
            self.flags.write = v;
            self
        }
        /// See std.
        pub fn append(&mut self, v: bool) -> &mut Self {
            self.flags.append = v;
            if v {
                self.flags.write = true;
            }
            self
        }
        /// See std.
        pub fn truncate(&mut self, v: bool) -> &mut Self {
            self.flags.trunc = v;
            self
        }
        /// See std.
        pub fn create(&mut self, v: bool) -> &mut Self {
            self.flags.create = v;
            self
        }
        /// See std.
        pub fn create_new(&mut self, v: bool) -> &mut Self {
            self.create_new = v;
            self
        }

        /// See std.
        pub fn open<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
            if !rt::in_sim() {
                let mut o = std::fs::OpenOptions::new();
                o.read(self.flags.read)
                    .write(self.flags.write)
                    .append(self.flags.append)
                    .truncate(self.flags.trunc)
                    .create(self.flags.create)
                    .create_new(self.create_new);
                std::os::unix::fs::OpenOptionsExt::custom_flags(&mut o, self.custom);
                std::os::unix::fs::OpenOptionsExt::mode(&mut o, self.mode);
                return o.open(path).map(|f| File(Repr::Real(f)));
            }
            let mut flags = self.flags;
            if self.create_new {
                flags.create = true;
                flags.excl = true;
            }
            flags.dsync |= self.custom & libc::O_DSYNC != 0;
            flags.sync |= self.custom & libc::O_SYNC != 0;
            flags.directory |= self.custom & libc::O_DIRECTORY != 0;
            match sys(Syscall::Open { path: sim_path(path.as_ref())?, flags })? {
                SysOk::Fd(fd) => Ok(File(Repr::Sim { fd, pid: rt::current_pid() })),
                v => unreachable!("open returned {v:?}"),
            }
        }
    }

    impl std::os::unix::fs::OpenOptionsExt for OpenOptions {
        fn mode(&mut self, mode: u32) -> &mut Self {
            self.mode = mode;
            self
        }
        fn custom_flags(&mut self, flags: i32) -> &mut Self {
            self.custom = flags;
            self
        }
    }

    /// `std::fs::Metadata`.
    #[derive(Clone, Debug)]
    pub enum Metadata {
        #[doc(hidden)]
        Real(std::fs::Metadata),
        #[doc(hidden)]
        Sim(Stat),
    }

    impl Metadata {
        /// See std.
        pub fn len(&self) -> u64 {
            match self {
                Metadata::Real(m) => m.len(),
                Metadata::Sim(s) => s.size,
            }
        }
        /// See std.
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }
        /// See std.
        pub fn is_dir(&self) -> bool {
            match self {
                Metadata::Real(m) => m.is_dir(),
                Metadata::Sim(s) => s.is_dir,
            }
        }
        /// See std.
        pub fn is_file(&self) -> bool {
            match self {
                Metadata::Real(m) => m.is_file(),
                Metadata::Sim(s) => !s.is_dir,
            }
        }
    }

    /// `std::fs::DirEntry`.
    #[derive(Debug)]
    pub struct DirEntry {
        dir: PathBuf,
        name: String,
        is_dir: bool,
        real: Option<std::fs::DirEntry>,
    }

    impl DirEntry {
        /// See std.
        pub fn path(&self) -> PathBuf {
            match &self.real {
                Some(e) => e.path(),
                None => self.dir.join(&self.name),
            }
        }
        /// See std.
        pub fn file_name(&self) -> OsString {
            match &self.real {
                Some(e) => e.file_name(),
                None => OsString::from(&self.name),
            }
        }
        /// See std.
        pub fn metadata(&self) -> io::Result<Metadata> {
            match &self.real {
                Some(e) => e.metadata().map(Metadata::Real),
                None => metadata(self.path()),
            }
        }
        /// Whether the entry is a directory (std's `file_type().is_dir()`).
        pub fn is_dir(&self) -> bool {
            match &self.real {
                Some(e) => e.file_type().map(|t| t.is_dir()).unwrap_or(false),
                None => self.is_dir,
            }
        }
    }

    /// `std::fs::ReadDir`.
    #[derive(Debug)]
    pub struct ReadDir(std::vec::IntoIter<io::Result<DirEntry>>);

    impl Iterator for ReadDir {
        type Item = io::Result<DirEntry>;
        fn next(&mut self) -> Option<Self::Item> {
            self.0.next()
        }
    }

    /// See std. Under the simulation the order is the kernel's (drawn with the
    /// `ReaddirShuffle` class), as readdir(3) promises no order.
    pub fn read_dir<P: AsRef<Path>>(path: P) -> io::Result<ReadDir> {
        let path = path.as_ref();
        if !rt::in_sim() {
            let v: Vec<io::Result<DirEntry>> = std::fs::read_dir(path)?
                .map(|e| e.map(|e| DirEntry { dir: PathBuf::new(), name: String::new(), is_dir: false, real: Some(e) }))
                .collect();
            return Ok(ReadDir(v.into_iter()));
        }
        match sys(Syscall::ReadDir { path: sim_path(path)? })? {
            SysOk::Entries(es) => Ok(ReadDir(
                es.into_iter()
                    .map(|e| Ok(DirEntry { dir: path.to_path_buf(), name: e.name, is_dir: e.is_dir, real: None }))
                    .collect::<Vec<_>>()
                    .into_iter(),
            )),
            v => unreachable!("readdir returned {v:?}"),
        }
    }

    /// See std.
    pub fn metadata<P: AsRef<Path>>(path: P) -> io::Result<Metadata> {
        if !rt::in_sim() {
            return std::fs::metadata(path).map(Metadata::Real);
        }
        match sys(Syscall::Stat { path: sim_path(path.as_ref())? })? {
            SysOk::Stat(s) => Ok(Metadata::Sim(s)),
            v => unreachable!("stat returned {v:?}"),
        }
    }

    /// See std.
    pub fn exists<P: AsRef<Path>>(path: P) -> io::Result<bool> {
        match metadata(path) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// See std.
    pub fn read<P: AsRef<Path>>(path: P) -> io::Result<Vec<u8>> {
        let mut f = File::open(path)?;
        let mut v = Vec::new();
        f.read_to_end(&mut v)?;
        Ok(v)
    }

    /// See std.
    pub fn read_to_string<P: AsRef<Path>>(path: P) -> io::Result<String> {
        let mut f = File::open(path)?;
        let mut s = String::new();
        f.read_to_string(&mut s)?;
        Ok(s)
    }

    /// See std.
    pub fn write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
        File::create(path)?.write_all(contents.as_ref())
    }

    fn path_call<P: AsRef<Path>>(
        path: P,
        real: impl FnOnce(&Path) -> io::Result<()>,
        sim: impl FnOnce(String) -> Syscall,
    ) -> io::Result<()> {
        if !rt::in_sim() {
            return real(path.as_ref());
        }
        sys(sim(sim_path(path.as_ref())?)).map(drop)
    }

    /// See std. Durable only after the parent directory is synced ([`super::sync_dir`]).
    pub fn rename<P: AsRef<Path>, Q: AsRef<Path>>(from: P, to: Q) -> io::Result<()> {
        if !rt::in_sim() {
            return std::fs::rename(from, to);
        }
        sys(Syscall::Rename { from: sim_path(from.as_ref())?, to: sim_path(to.as_ref())? }).map(drop)
    }

    /// See std.
    pub fn remove_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
        path_call(path, |p| std::fs::remove_file(p), |path| Syscall::Unlink { path })
    }

    /// See std.
    pub fn create_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
        path_call(path, |p| std::fs::create_dir(p), |path| Syscall::Mkdir { path })
    }

    /// See std.
    pub fn remove_dir<P: AsRef<Path>>(path: P) -> io::Result<()> {
        path_call(path, |p| std::fs::remove_dir(p), |path| Syscall::Rmdir { path })
    }

    /// See std.
    pub fn create_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
        if !rt::in_sim() {
            return std::fs::create_dir_all(path);
        }
        let full = sim_path(path.as_ref())?;
        let mut cur = String::new();
        for part in full.split('/').filter(|s| !s.is_empty()) {
            cur.push('/');
            cur.push_str(part);
            match sys(Syscall::Mkdir { path: cur.clone() }) {
                Ok(_) => {}
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// See std; removes every file and directory below `path` too.
    pub fn remove_dir_all<P: AsRef<Path>>(path: P) -> io::Result<()> {
        if !rt::in_sim() {
            return std::fs::remove_dir_all(path);
        }
        for e in read_dir(path.as_ref())? {
            let e = e?;
            if e.is_dir() {
                remove_dir_all(e.path())?;
            } else {
                remove_file(e.path())?;
            }
        }
        remove_dir(path)
    }

    pub(super) fn sync_dir_sim(path: &Path) -> io::Result<()> {
        let mut o = OpenOptions::new();
        o.read(true);
        o.flags.directory = true;
        o.open(path)?.sync_all()
    }
}
