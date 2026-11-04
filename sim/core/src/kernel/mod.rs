//! A Linux kernel model: page cache, durability, crashes, locks and mmap.
//!
//! A synchronous state machine. [`Kernel::syscall`] applies one call of one
//! process; a call that must wait returns [`SysResult::Block`] and is retried by
//! the driver when its key is woken (see [`Kernel::take_woken`]). Every fault is a
//! draw through the [`Choices`](crate::Choices) port and only for classes the
//! run's [`Swarm`] enabled, so a fault Linux cannot produce is never injected.
//! Rules are cited where they are implemented; `docs/dst.md` lists them.

mod buf;
mod file;
mod locks;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use file::{FileData, Level};
use locks::{FcntlLock, FlockHolder};

use crate::sched::WaitKey;
use crate::{Choices, Fault, Pid, Swarm, Trace};

/// An inode number.
pub type Ino = u64;
/// A file descriptor.
pub type Fd = i32;
/// The page size.
pub const PAGE: u64 = 4096;
/// The root directory's inode.
pub const ROOT: Ino = 1;

/// A Linux errno.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Errno(pub i32);

macro_rules! errnos {
    ($($name:ident = $v:expr),* $(,)?) => {
        impl Errno {
            $(#[allow(missing_docs)] pub const $name: Errno = Errno($v);)*
            /// The symbolic name.
            pub fn name(self) -> &'static str {
                match self.0 { $($v => stringify!($name),)* _ => "E?" }
            }
        }
    };
}

errnos! {
    EPERM = 1, ENOENT = 2, EINTR = 4, EIO = 5, EBADF = 9, EAGAIN = 11, ENOMEM = 12,
    EBUSY = 16, EEXIST = 17, EXDEV = 18, ENOTDIR = 20, EISDIR = 21, EINVAL = 22,
    EFBIG = 27, ENOSPC = 28, ESPIPE = 29, EROFS = 30, ENOSYS = 38, ENOTEMPTY = 39,
    EDEADLK = 35,
}

impl Errno {
    /// flock's `EWOULDBLOCK` is `EAGAIN` on Linux.
    pub const EWOULDBLOCK: Errno = Errno::EAGAIN;
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.name())
    }
}

/// Which kernel and file system semantics to model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Profile {
    /// Linux; a directory operation is durable only after `fsync` of its parent.
    #[default]
    LinuxStrict,
    /// Linux ext4: any `fsync` commits the jbd2 transaction, so every earlier
    /// metadata operation becomes durable with it.
    LinuxExt4,
    /// macOS: `fsync` reaches the device cache only; `F_FULLFSYNC` reaches media.
    MacOs,
}

/// `open(2)` flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OpenFlags {
    /// `O_RDONLY`/`O_RDWR`.
    pub read: bool,
    /// `O_WRONLY`/`O_RDWR`.
    pub write: bool,
    /// `O_CREAT`.
    pub create: bool,
    /// `O_EXCL`.
    pub excl: bool,
    /// `O_TRUNC`.
    pub trunc: bool,
    /// `O_APPEND`.
    pub append: bool,
    /// `O_DSYNC`.
    pub dsync: bool,
    /// `O_SYNC`.
    pub sync: bool,
    /// `O_DIRECTORY`.
    pub directory: bool,
}

/// `lseek(2)` whence and offset.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Whence {
    /// `SEEK_SET`.
    Set(u64),
    /// `SEEK_CUR`.
    Cur(i64),
    /// `SEEK_END`.
    End(i64),
}

/// `flock(2)` operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FlockOp {
    /// `LOCK_SH`.
    Shared,
    /// `LOCK_EX`.
    Exclusive,
    /// `LOCK_UN`.
    Unlock,
}

/// `fcntl(2)` record lock type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockKind {
    /// `F_RDLCK`.
    Read,
    /// `F_WRLCK`.
    Write,
}

/// A syscall, in the model's vocabulary (shared by the native facade and WASI).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Syscall {
    /// `open(path, flags)`.
    Open { path: String, flags: OpenFlags },
    /// `close(fd)`.
    Close { fd: Fd },
    /// `read(fd, len)`.
    Read { fd: Fd, len: usize },
    /// `pread(fd, len, offset)`.
    Pread { fd: Fd, len: usize, offset: u64 },
    /// `write(fd, data)`.
    Write { fd: Fd, data: Vec<u8> },
    /// `pwrite(fd, data, offset)`.
    Pwrite { fd: Fd, data: Vec<u8>, offset: u64 },
    /// `lseek(fd, ..)`.
    Seek { fd: Fd, whence: Whence },
    /// `fsync(fd)`.
    Fsync { fd: Fd },
    /// `fdatasync(fd)`.
    Fdatasync { fd: Fd },
    /// `fcntl(fd, F_FULLFSYNC)` (macOS); `fsync` elsewhere.
    FullFsync { fd: Fd },
    /// `ftruncate(fd, len)`.
    Ftruncate { fd: Fd, len: u64 },
    /// `fstat(fd)`.
    Fstat { fd: Fd },
    /// `stat(path)`.
    Stat { path: String },
    /// `rename(from, to)`.
    Rename { from: String, to: String },
    /// `unlink(path)`.
    Unlink { path: String },
    /// `mkdir(path)`.
    Mkdir { path: String },
    /// `rmdir(path)`.
    Rmdir { path: String },
    /// `opendir` + `readdir` to the end.
    ReadDir { path: String },
    /// `flock(fd, op | LOCK_NB?)`.
    Flock { fd: Fd, op: FlockOp, nonblock: bool },
    /// `fcntl(fd, F_SETLK/F_SETLKW, ..)`; `kind: None` unlocks. `len == 0` means to EOF and beyond.
    FcntlLock { fd: Fd, kind: Option<LockKind>, start: u64, len: u64, wait: bool },
    /// `mmap(NULL, len, PROT_READ|PROT_WRITE?, MAP_SHARED|MAP_PRIVATE, fd, offset)`.
    Mmap { fd: Fd, offset: u64, len: usize, shared: bool },
    /// `munmap` of a mapping.
    Munmap { map: u64 },
    /// `msync(MS_SYNC)` of `[offset, offset+len)` within a mapping.
    Msync { map: u64, offset: u64, len: u64 },
}

impl Syscall {
    /// The syscall's name, for the trace.
    pub fn name(&self) -> &'static str {
        match self {
            Syscall::Open { .. } => "open",
            Syscall::Close { .. } => "close",
            Syscall::Read { .. } => "read",
            Syscall::Pread { .. } => "pread",
            Syscall::Write { .. } => "write",
            Syscall::Pwrite { .. } => "pwrite",
            Syscall::Seek { .. } => "lseek",
            Syscall::Fsync { .. } => "fsync",
            Syscall::Fdatasync { .. } => "fdatasync",
            Syscall::FullFsync { .. } => "F_FULLFSYNC",
            Syscall::Ftruncate { .. } => "ftruncate",
            Syscall::Fstat { .. } => "fstat",
            Syscall::Stat { .. } => "stat",
            Syscall::Rename { .. } => "rename",
            Syscall::Unlink { .. } => "unlink",
            Syscall::Mkdir { .. } => "mkdir",
            Syscall::Rmdir { .. } => "rmdir",
            Syscall::ReadDir { .. } => "readdir",
            Syscall::Flock { .. } => "flock",
            Syscall::FcntlLock { .. } => "fcntl_lock",
            Syscall::Mmap { .. } => "mmap",
            Syscall::Munmap { .. } => "munmap",
            Syscall::Msync { .. } => "msync",
        }
    }

    /// Virtual time the call costs, in nanoseconds.
    pub fn cost_ns(&self) -> u64 {
        match self {
            Syscall::Fsync { .. } | Syscall::Fdatasync { .. } | Syscall::FullFsync { .. } | Syscall::Msync { .. } => {
                1_000_000
            }
            _ => 10_000,
        }
    }
}

/// `stat` result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    /// Inode number.
    pub ino: Ino,
    /// Size in bytes (0 for a directory).
    pub size: u64,
    /// Whether it is a directory.
    pub is_dir: bool,
}

/// One directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// Name within the directory.
    pub name: String,
    /// Inode number.
    pub ino: Ino,
    /// Whether it is a directory.
    pub is_dir: bool,
}

/// A successful syscall's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SysOk {
    /// No value.
    Unit,
    /// A new descriptor.
    Fd(Fd),
    /// Bytes read.
    Bytes(Vec<u8>),
    /// Bytes written.
    Count(usize),
    /// A file offset.
    Offset(u64),
    /// File status.
    Stat(Stat),
    /// Directory entries.
    Entries(Vec<DirEntry>),
    /// A mapping: its id, the address of its first byte, its length.
    Map { id: u64, addr: usize, len: usize },
}

/// A syscall's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SysResult {
    /// It succeeded.
    Ok(SysOk),
    /// It failed with an errno.
    Err(Errno),
    /// It must wait on a key; retry the same call once the key is woken.
    Block(WaitKey),
}

/// How a crash happens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashKind {
    /// kill -9 of one process: nothing durable changes.
    Kill9(Pid),
    /// Power loss: page cache and device cache are lost.
    PowerLoss,
    /// Kernel crash: page cache is lost, the device cache survives.
    OsCrash,
}

/// What the kernel needs from the driver for one call: the choice port, the
/// run's swarm, and where to record what happened.
pub struct Env<'a> {
    /// The choice port.
    pub choices: &'a dyn Choices,
    /// Enabled fault classes and rates.
    pub swarm: &'a Swarm,
    /// The trace.
    pub trace: &'a mut Trace,
    /// The current scheduler step, for the trace.
    pub step: u64,
    /// The current virtual time, for the trace.
    pub t_ns: u64,
    /// Who is calling, for the trace.
    pub who: &'a str,
}

impl Env<'_> {
    fn log(&mut self, text: String) {
        let line = format!("{}: {text}", self.who);
        self.trace.push(self.step, self.t_ns, line);
    }
}

/// Kernel parameters.
#[derive(Clone, Debug)]
pub struct KernelConfig {
    /// Semantics profile.
    pub profile: Profile,
    /// The device's atomic write unit in bytes (512, or 4096 with NVMe AWUPF covering a page).
    pub atomic_write_unit: u64,
    /// File system capacity in bytes.
    pub capacity: u64,
    /// Bytes reserved for a mapped file's growth (it never moves once mapped).
    pub mmap_reserve: usize,
}

impl Default for KernelConfig {
    fn default() -> Self {
        KernelConfig {
            profile: Profile::LinuxStrict,
            atomic_write_unit: 512,
            capacity: 1 << 30,
            mmap_reserve: 64 << 20,
        }
    }
}

#[derive(Debug)]
enum Kind {
    File(FileData),
    Dir(BTreeMap<String, Ino>),
}

#[derive(Debug)]
struct Inode {
    kind: Kind,
    links: u32,
    opens: u32,
    flock: Vec<FlockHolder>,
    fcntl: Vec<FcntlLock>,
}

impl Inode {
    fn new(kind: Kind) -> Inode {
        Inode {
            kind,
            links: 0,
            opens: 0,
            flock: Vec::new(),
            fcntl: Vec::new(),
        }
    }

    fn is_dir(&self) -> bool {
        matches!(self.kind, Kind::Dir(_))
    }
}

#[derive(Clone, Debug)]
enum DirOpKind {
    Link { dir: Ino, name: String, ino: Ino },
    Unlink { dir: Ino, name: String, ino: Ino },
    Rename { from_dir: Ino, from: String, to_dir: Ino, to: String, ino: Ino },
}

#[derive(Clone, Debug)]
struct DirOp {
    kind: DirOpKind,
    need: Vec<Ino>,
    synced_dev: BTreeSet<Ino>,
    synced_media: BTreeSet<Ino>,
    on_dev: bool,
    on_media: bool,
}

type Ns = BTreeMap<Ino, BTreeMap<String, Ino>>;

fn apply(ns: &mut Ns, op: &DirOpKind) {
    match op {
        DirOpKind::Link { dir, name, ino } => {
            ns.entry(*dir).or_default().insert(name.clone(), *ino);
        }
        DirOpKind::Unlink { dir, name, ino } => {
            let d = ns.entry(*dir).or_default();
            if d.get(name) == Some(ino) {
                d.remove(name);
            }
        }
        DirOpKind::Rename { from_dir, from, to_dir, to, ino } => {
            let d = ns.entry(*from_dir).or_default();
            if d.get(from) == Some(ino) {
                d.remove(from);
            }
            ns.entry(*to_dir).or_default().insert(to.clone(), *ino);
        }
    }
}

#[derive(Debug)]
struct Ofd {
    ino: Ino,
    offset: u64,
    flags: OpenFlags,
    errseq: u32,
    refs: u32,
}

#[derive(Debug)]
struct Mapping {
    ino: Ino,
    pid: Pid,
    offset: u64,
    len: usize,
    private: Option<Vec<u8>>,
}

/// The simulated kernel and its one file system.
#[derive(Debug)]
pub struct Kernel {
    cfg: KernelConfig,
    inodes: BTreeMap<Ino, Inode>,
    next_ino: Ino,
    dir_ops: Vec<DirOp>,
    media_ns: Ns,
    dev_ns: Ns,
    ofds: BTreeMap<u64, Ofd>,
    next_ofd: u64,
    fds: BTreeMap<Pid, BTreeMap<Fd, u64>>,
    maps: BTreeMap<u64, Mapping>,
    next_map: u64,
    woken: Vec<WaitKey>,
}

impl Kernel {
    /// A kernel with an empty file system (just `/`).
    pub fn new(cfg: KernelConfig) -> Kernel {
        let mut inodes = BTreeMap::new();
        let mut root = Inode::new(Kind::Dir(BTreeMap::new()));
        root.links = 1;
        inodes.insert(ROOT, root);
        let mut ns = Ns::new();
        ns.insert(ROOT, BTreeMap::new());
        Kernel {
            cfg,
            inodes,
            next_ino: ROOT + 1,
            dir_ops: Vec::new(),
            media_ns: ns.clone(),
            dev_ns: ns,
            ofds: BTreeMap::new(),
            next_ofd: 1,
            fds: BTreeMap::new(),
            maps: BTreeMap::new(),
            next_map: 1,
            woken: Vec::new(),
        }
    }

    /// The configuration.
    pub fn config(&self) -> &KernelConfig {
        &self.cfg
    }

    /// Wait keys made ready since the last call; the driver wakes their waiters.
    pub fn take_woken(&mut self) -> Vec<WaitKey> {
        std::mem::take(&mut self.woken)
    }

    // ---- paths ----

    fn components(path: &str) -> Result<Vec<&str>, Errno> {
        let mut out = Vec::new();
        for c in path.split('/') {
            match c {
                "" | "." => {}
                ".." => {
                    out.pop();
                }
                c => out.push(c),
            }
        }
        Ok(out)
    }

    fn dir_entries(&self, ino: Ino) -> Result<&BTreeMap<String, Ino>, Errno> {
        match self.inodes.get(&ino).map(|i| &i.kind) {
            Some(Kind::Dir(e)) => Ok(e),
            Some(Kind::File(_)) => Err(Errno::ENOTDIR),
            None => Err(Errno::ENOENT),
        }
    }

    fn dir_entries_mut(&mut self, ino: Ino) -> Result<&mut BTreeMap<String, Ino>, Errno> {
        match self.inodes.get_mut(&ino).map(|i| &mut i.kind) {
            Some(Kind::Dir(e)) => Ok(e),
            Some(Kind::File(_)) => Err(Errno::ENOTDIR),
            None => Err(Errno::ENOENT),
        }
    }

    fn lookup(&self, path: &str) -> Result<Ino, Errno> {
        let mut cur = ROOT;
        for c in Self::components(path)? {
            cur = *self.dir_entries(cur)?.get(c).ok_or(Errno::ENOENT)?;
        }
        Ok(cur)
    }

    /// The parent directory and final name of `path`.
    fn parent(&self, path: &str) -> Result<(Ino, String), Errno> {
        let comps = Self::components(path)?;
        let (name, dirs) = comps.split_last().ok_or(Errno::EBUSY)?;
        let mut cur = ROOT;
        for c in dirs {
            cur = *self.dir_entries(cur)?.get(*c).ok_or(Errno::ENOENT)?;
        }
        self.dir_entries(cur)?;
        Ok((cur, (*name).to_owned()))
    }

    fn file(&self, ino: Ino) -> Result<&FileData, Errno> {
        match self.inodes.get(&ino).map(|i| &i.kind) {
            Some(Kind::File(f)) => Ok(f),
            Some(Kind::Dir(_)) => Err(Errno::EISDIR),
            None => Err(Errno::EBADF),
        }
    }

    fn file_mut(&mut self, ino: Ino) -> Result<&mut FileData, Errno> {
        match self.inodes.get_mut(&ino).map(|i| &mut i.kind) {
            Some(Kind::File(f)) => Ok(f),
            Some(Kind::Dir(_)) => Err(Errno::EISDIR),
            None => Err(Errno::EBADF),
        }
    }

    fn ofd_of(&self, pid: Pid, fd: Fd) -> Result<u64, Errno> {
        self.fds.get(&pid).and_then(|t| t.get(&fd)).copied().ok_or(Errno::EBADF)
    }

    fn record(&mut self, kind: DirOpKind) {
        let need = match &kind {
            DirOpKind::Link { dir, .. } | DirOpKind::Unlink { dir, .. } => vec![*dir],
            DirOpKind::Rename { from_dir, to_dir, .. } if from_dir == to_dir => vec![*from_dir],
            DirOpKind::Rename { from_dir, to_dir, .. } => vec![*from_dir, *to_dir],
        };
        self.dir_ops.push(DirOp {
            kind,
            need,
            synced_dev: BTreeSet::new(),
            synced_media: BTreeSet::new(),
            on_dev: false,
            on_media: false,
        });
    }

    /// Mark directory operations durable at `level`: those touching `dir`, or all
    /// of them (`dir == None`, an ext4 journal commit).
    fn sync_dir_ops(&mut self, dir: Option<Ino>, level: Level) {
        for op in &mut self.dir_ops {
            let touches = |d: &Ino| dir.is_none_or(|x| x == *d);
            for d in op.need.clone().iter().filter(|d| touches(d)) {
                op.synced_dev.insert(*d);
                if level == Level::Media {
                    op.synced_media.insert(*d);
                }
            }
            if !op.on_dev && op.need.iter().all(|d| op.synced_dev.contains(d)) {
                op.on_dev = true;
                apply(&mut self.dev_ns, &op.kind);
            }
            if !op.on_media && op.need.iter().all(|d| op.synced_media.contains(d)) {
                op.on_media = true;
                apply(&mut self.media_ns, &op.kind);
            }
        }
        self.dir_ops.retain(|op| !op.on_media);
    }

    fn used_pages(&self) -> u64 {
        self.inodes
            .values()
            .filter(|i| i.links > 0 || i.opens > 0)
            .map(|i| match &i.kind {
                Kind::File(f) => f.size().div_ceil(PAGE),
                Kind::Dir(_) => 0,
            })
            .sum()
    }

    fn maybe_free(&mut self, ino: Ino) {
        let _ = ino;
    }

    // ---- processes ----

    /// Register a process's (empty) descriptor table.
    pub fn spawn(&mut self, pid: Pid) {
        self.fds.entry(pid).or_default();
    }

    /// A process exited or was killed: close its descriptors (releasing its
    /// flock and fcntl locks) and drop its mappings.
    pub fn exit(&mut self, pid: Pid) {
        let fds: Vec<Fd> = self.fds.get(&pid).map(|t| t.keys().copied().collect()).unwrap_or_default();
        for fd in fds {
            let _ = self.close(pid, fd);
        }
        self.fds.remove(&pid);
        let maps: Vec<u64> = self.maps.iter().filter(|(_, m)| m.pid == pid).map(|(id, _)| *id).collect();
        for id in maps {
            self.unmap(id);
        }
    }

    // ---- the syscall entry ----

    /// Apply one syscall of `pid`.
    pub fn syscall(&mut self, pid: Pid, call: &Syscall, env: &mut Env<'_>) -> SysResult {
        let r = match call {
            Syscall::Open { path, flags } => self.open(pid, path, *flags).map(SysOk::Fd),
            Syscall::Close { fd } => self.close(pid, *fd).map(|_| SysOk::Unit),
            Syscall::Read { fd, len } => self.read(pid, *fd, *len, None).map(SysOk::Bytes),
            Syscall::Pread { fd, len, offset } => self.read(pid, *fd, *len, Some(*offset)).map(SysOk::Bytes),
            Syscall::Write { fd, data } => self.write(pid, *fd, data, None, env).map(SysOk::Count),
            Syscall::Pwrite { fd, data, offset } => self.write(pid, *fd, data, Some(*offset), env).map(SysOk::Count),
            Syscall::Seek { fd, whence } => self.seek(pid, *fd, *whence).map(SysOk::Offset),
            Syscall::Fsync { fd } | Syscall::Fdatasync { fd } => {
                let level = if self.cfg.profile == Profile::MacOs { Level::Dev } else { Level::Media };
                self.fsync(pid, *fd, level, env).map(|_| SysOk::Unit)
            }
            Syscall::FullFsync { fd } => self.fsync(pid, *fd, Level::Media, env).map(|_| SysOk::Unit),
            Syscall::Ftruncate { fd, len } => self.ftruncate(pid, *fd, *len).map(|_| SysOk::Unit),
            Syscall::Fstat { fd } => self.ofd_of(pid, *fd).and_then(|o| self.stat_ino(self.ofds[&o].ino)).map(SysOk::Stat),
            Syscall::Stat { path } => self.lookup(path).and_then(|i| self.stat_ino(i)).map(SysOk::Stat),
            Syscall::Rename { from, to } => self.rename(from, to).map(|_| SysOk::Unit),
            Syscall::Unlink { path } => self.unlink(path, false).map(|_| SysOk::Unit),
            Syscall::Rmdir { path } => self.unlink(path, true).map(|_| SysOk::Unit),
            Syscall::Mkdir { path } => self.mkdir(path).map(|_| SysOk::Unit),
            Syscall::ReadDir { path } => self.readdir(path, env).map(SysOk::Entries),
            Syscall::Flock { fd, op, nonblock } => return self.flock(pid, *fd, *op, *nonblock),
            Syscall::FcntlLock { fd, kind, start, len, wait } => {
                return self.fcntl_lock(pid, *fd, *kind, *start, *len, *wait);
            }
            Syscall::Mmap { fd, offset, len, shared } => self.mmap(pid, *fd, *offset, *len, *shared),
            Syscall::Munmap { map } => {
                if self.maps.get(map).is_some_and(|m| m.pid == pid) {
                    self.unmap(*map);
                    Ok(SysOk::Unit)
                } else {
                    Err(Errno::EINVAL)
                }
            }
            Syscall::Msync { map, offset, len } => self.msync(pid, *map, *offset, *len).map(|_| SysOk::Unit),
        };
        match r {
            Ok(v) => SysResult::Ok(v),
            Err(e) => SysResult::Err(e),
        }
    }

    fn stat_ino(&self, ino: Ino) -> Result<Stat, Errno> {
        let i = self.inodes.get(&ino).ok_or(Errno::ENOENT)?;
        Ok(Stat {
            ino,
            size: match &i.kind {
                Kind::File(f) => f.size(),
                Kind::Dir(_) => 0,
            },
            is_dir: i.is_dir(),
        })
    }

    fn open(&mut self, pid: Pid, path: &str, flags: OpenFlags) -> Result<Fd, Errno> {
        let (dir, name) = match self.parent(path) {
            Ok(p) => p,
            // Opening "/" itself.
            Err(Errno::EBUSY) => (ROOT, String::new()),
            Err(e) => return Err(e),
        };
        let existing = if name.is_empty() { Some(ROOT) } else { self.dir_entries(dir)?.get(&name).copied() };
        let ino = match existing {
            Some(ino) => {
                if flags.create && flags.excl {
                    return Err(Errno::EEXIST);
                }
                let is_dir = self.inodes[&ino].is_dir();
                if is_dir && (flags.write || flags.trunc) {
                    return Err(Errno::EISDIR);
                }
                if flags.directory && !is_dir {
                    return Err(Errno::ENOTDIR);
                }
                if flags.trunc && flags.write && !is_dir {
                    self.file_mut(ino)?.set_size(0)?;
                }
                ino
            }
            None => {
                if !flags.create {
                    return Err(Errno::ENOENT);
                }
                let ino = self.next_ino;
                self.next_ino += 1;
                let mut inode = Inode::new(Kind::File(FileData::new()));
                inode.links = 1;
                self.inodes.insert(ino, inode);
                self.dir_entries_mut(dir)?.insert(name.clone(), ino);
                self.record(DirOpKind::Link { dir, name, ino });
                ino
            }
        };
        let errseq = match &self.inodes[&ino].kind {
            // errseq_sample: an error nobody has seen yet is reported to a new
            // opener too (lib/errseq.c, since Linux 4.16).
            Kind::File(f) if !f.wb_seen => f.wb_err.wrapping_sub(1),
            Kind::File(f) => f.wb_err,
            Kind::Dir(_) => 0,
        };
        let id = self.next_ofd;
        self.next_ofd += 1;
        self.ofds.insert(id, Ofd { ino, offset: 0, flags, errseq, refs: 1 });
        if let Some(i) = self.inodes.get_mut(&ino) {
            i.opens += 1;
        }
        let table = self.fds.entry(pid).or_default();
        // open(2): "the lowest-numbered file descriptor not currently open".
        let mut fd = 3;
        while table.contains_key(&fd) {
            fd += 1;
        }
        table.insert(fd, id);
        Ok(fd)
    }

    fn close(&mut self, pid: Pid, fd: Fd) -> Result<(), Errno> {
        let id = self.fds.get_mut(&pid).and_then(|t| t.remove(&fd)).ok_or(Errno::EBADF)?;
        let ino = self.ofds[&id].ino;
        // fcntl(2): record locks are released when the process closes *any*
        // descriptor referring to the file.
        if let Some(i) = self.inodes.get_mut(&ino) {
            let before = i.fcntl.len();
            i.fcntl.retain(|l| l.pid != pid);
            if i.fcntl.len() != before {
                self.woken.push(WaitKey::Fcntl(ino));
            }
        }
        let o = self.ofds.get_mut(&id).ok_or(Errno::EBADF)?;
        o.refs -= 1;
        if o.refs == 0 {
            self.ofds.remove(&id);
            // flock(2): a lock is released when all descriptors of its open file
            // description are closed.
            if let Some(i) = self.inodes.get_mut(&ino) {
                let before = i.flock.len();
                i.flock.retain(|h| h.ofd != id);
                if i.flock.len() != before {
                    self.woken.push(WaitKey::Flock(ino));
                }
                i.opens -= 1;
            }
            self.maybe_free(ino);
        }
        Ok(())
    }

    fn read(&mut self, pid: Pid, fd: Fd, len: usize, at: Option<u64>) -> Result<Vec<u8>, Errno> {
        let id = self.ofd_of(pid, fd)?;
        let o = &self.ofds[&id];
        if !o.flags.read {
            return Err(Errno::EBADF);
        }
        let off = at.unwrap_or(o.offset);
        let f = self.file(o.ino)?;
        let bytes = f.cache.bytes();
        // A regular-file read is short only at EOF.
        let start = (off as usize).min(bytes.len());
        let end = start.saturating_add(len).min(bytes.len());
        let out = bytes[start..end].to_vec();
        if at.is_none() {
            self.ofds.get_mut(&id).ok_or(Errno::EBADF)?.offset = off + out.len() as u64;
        }
        Ok(out)
    }

    fn write(&mut self, pid: Pid, fd: Fd, data: &[u8], at: Option<u64>, env: &mut Env<'_>) -> Result<usize, Errno> {
        let id = self.ofd_of(pid, fd)?;
        let (ino, flags, cur) = {
            let o = &self.ofds[&id];
            (o.ino, o.flags, o.offset)
        };
        if !flags.write {
            return Err(Errno::EBADF);
        }
        let size = self.file(ino)?.size();
        // pwrite(2), BUGS: "on Linux, if a file is opened with O_APPEND, pwrite()
        // appends data to the end of the file, regardless of the value of offset."
        let off = if flags.append { size } else { at.unwrap_or(cur) };
        let mut n = data.len();
        let end = off + n as u64;
        let new_pages = end.div_ceil(PAGE).saturating_sub(size.div_ceil(PAGE));
        if new_pages > 0 && n > 0 {
            let free = (self.cfg.capacity / PAGE).saturating_sub(self.used_pages());
            if new_pages > free {
                let max_end = (size.div_ceil(PAGE) + free) * PAGE;
                n = max_end.saturating_sub(off).min(n as u64) as usize;
                if n == 0 {
                    env.log(format!("write fd{fd} ENOSPC (file system full)"));
                    return Err(Errno::ENOSPC);
                }
            }
            if env.swarm.fire(env.choices, Fault::Enospc, env.swarm.rates.enospc) {
                // write(2): "the number of bytes written may be less than count if
                // ... there is insufficient space on the underlying physical medium".
                if n > 1 && env.choices.coin("kernel.enospc_short", 0.5) {
                    n = env.choices.int("kernel.enospc_short_len", 1, n as u64 - 1) as usize;
                    env.log(format!("write fd{fd} short: {n} of {} bytes (ENOSPC)", data.len()));
                } else {
                    env.log(format!("write fd{fd} ENOSPC ({} bytes)", data.len()));
                    return Err(Errno::ENOSPC);
                }
            }
        }
        let f = self.file_mut(ino)?;
        f.write(off, &data[..n])?;
        if flags.dsync || flags.sync {
            let level = if self.cfg.profile == Profile::MacOs { Level::Dev } else { Level::Media };
            let pages: Vec<u64> = if n == 0 { Vec::new() } else { (off / PAGE..=(off + n as u64 - 1) / PAGE).collect() };
            self.file_mut(ino)?.sync(level, Some(&pages));
        }
        if (at.is_none() || flags.append)
            && let Some(o) = self.ofds.get_mut(&id)
                && at.is_none() {
                    o.offset = off + n as u64;
                }
        Ok(n)
    }

    fn seek(&mut self, pid: Pid, fd: Fd, whence: Whence) -> Result<u64, Errno> {
        let id = self.ofd_of(pid, fd)?;
        let ino = self.ofds[&id].ino;
        let size = match &self.inodes[&ino].kind {
            Kind::File(f) => f.size(),
            Kind::Dir(_) => 0,
        };
        let o = self.ofds.get_mut(&id).ok_or(Errno::EBADF)?;
        let new = match whence {
            Whence::Set(x) => Some(x),
            Whence::Cur(d) => o.offset.checked_add_signed(d),
            Whence::End(d) => size.checked_add_signed(d),
        }
        .ok_or(Errno::EINVAL)?;
        o.offset = new;
        Ok(new)
    }

    fn fsync(&mut self, pid: Pid, fd: Fd, level: Level, env: &mut Env<'_>) -> Result<(), Errno> {
        let id = self.ofd_of(pid, fd)?;
        let ino = self.ofds[&id].ino;
        if self.inodes[&ino].is_dir() {
            self.sync_dir_ops(Some(ino), level);
            if self.cfg.profile == Profile::LinuxExt4 {
                self.sync_dir_ops(None, level);
            }
            return Ok(());
        }
        let profile = self.cfg.profile;
        let f = self.file_mut(ino)?;
        if f.is_dirty() {
            let grows = f.size() > f.media.len() as u64;
            if env.swarm.fire(env.choices, Fault::FsyncEio, env.swarm.rates.fsync_eio) {
                let what = f.fail_writeback(level, Errno::EIO, env);
                env.log(format!("fsync fd{fd} writeback error EIO: {what}"));
            } else if grows && env.swarm.fire(env.choices, Fault::EnospcAtFsync, env.swarm.rates.enospc_at_fsync) {
                let what = f.fail_writeback(level, Errno::ENOSPC, env);
                env.log(format!("fsync fd{fd} delayed allocation ENOSPC: {what}"));
            } else {
                f.sync(level, None);
            }
        }
        if profile == Profile::LinuxExt4 {
            self.sync_dir_ops(None, level);
        }
        let (wb_err, code) = {
            let f = self.file_mut(ino)?;
            (f.wb_err, f.wb_err_code)
        };
        let o = self.ofds.get_mut(&id).ok_or(Errno::EBADF)?;
        // errseq_check_and_advance (lib/errseq.c): report each error once per
        // description that sampled before it.
        if o.errseq != wb_err {
            o.errseq = wb_err;
            self.file_mut(ino)?.wb_seen = true;
            return Err(code);
        }
        Ok(())
    }

    fn ftruncate(&mut self, pid: Pid, fd: Fd, len: u64) -> Result<(), Errno> {
        let id = self.ofd_of(pid, fd)?;
        let o = &self.ofds[&id];
        if !o.flags.write {
            return Err(Errno::EINVAL);
        }
        let ino = o.ino;
        let size = self.file(ino)?.size();
        if len.div_ceil(PAGE) > size.div_ceil(PAGE) {
            let free = (self.cfg.capacity / PAGE).saturating_sub(self.used_pages());
            if len.div_ceil(PAGE) - size.div_ceil(PAGE) > free {
                return Err(Errno::ENOSPC);
            }
        }
        self.file_mut(ino)?.set_size(len)
    }

    fn is_ancestor(&self, anc: Ino, mut ino: Ino) -> bool {
        // Walk up by searching parents (directories are few in a simulation).
        loop {
            if ino == anc {
                return true;
            }
            if ino == ROOT {
                return false;
            }
            let parent = self.inodes.iter().find_map(|(p, i)| match &i.kind {
                Kind::Dir(e) if e.values().any(|c| *c == ino) => Some(*p),
                _ => None,
            });
            match parent {
                Some(p) => ino = p,
                None => return false,
            }
        }
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), Errno> {
        let (fd_dir, fname) = self.parent(from)?;
        let ino = *self.dir_entries(fd_dir)?.get(&fname).ok_or(Errno::ENOENT)?;
        let (td_dir, tname) = self.parent(to)?;
        let replaced = self.dir_entries(td_dir)?.get(&tname).copied();
        if replaced == Some(ino) {
            // rename(2): "If oldpath and newpath are existing hard links referring
            // to the same file, then rename() does nothing".
            return Ok(());
        }
        let src_dir = self.inodes[&ino].is_dir();
        if src_dir && self.is_ancestor(ino, td_dir) {
            return Err(Errno::EINVAL);
        }
        if let Some(r) = replaced {
            match (&self.inodes[&r].kind, src_dir) {
                (Kind::Dir(_), false) => return Err(Errno::EISDIR),
                (Kind::File(_), true) => return Err(Errno::ENOTDIR),
                (Kind::Dir(e), true) if !e.is_empty() => return Err(Errno::ENOTEMPTY),
                _ => {}
            }
        }
        // rename(2): "If newpath already exists, it will be atomically replaced, so
        // that there is no point at which another process attempting to access
        // newpath will find it missing."
        self.dir_entries_mut(fd_dir)?.remove(&fname);
        self.dir_entries_mut(td_dir)?.insert(tname.clone(), ino);
        if let Some(r) = replaced {
            if let Some(i) = self.inodes.get_mut(&r) {
                i.links -= 1;
            }
            self.maybe_free(r);
        }
        self.record(DirOpKind::Rename { from_dir: fd_dir, from: fname, to_dir: td_dir, to: tname, ino });
        Ok(())
    }

    fn unlink(&mut self, path: &str, dir: bool) -> Result<(), Errno> {
        let (parent, name) = self.parent(path)?;
        let ino = *self.dir_entries(parent)?.get(&name).ok_or(Errno::ENOENT)?;
        match (&self.inodes[&ino].kind, dir) {
            (Kind::Dir(_), false) => return Err(Errno::EISDIR),
            (Kind::File(_), true) => return Err(Errno::ENOTDIR),
            (Kind::Dir(e), true) if !e.is_empty() => return Err(Errno::ENOTEMPTY),
            _ => {}
        }
        self.dir_entries_mut(parent)?.remove(&name);
        if let Some(i) = self.inodes.get_mut(&ino) {
            i.links -= 1;
        }
        self.maybe_free(ino);
        self.record(DirOpKind::Unlink { dir: parent, name, ino });
        Ok(())
    }

    fn mkdir(&mut self, path: &str) -> Result<(), Errno> {
        let (parent, name) = self.parent(path)?;
        if self.dir_entries(parent)?.contains_key(&name) {
            return Err(Errno::EEXIST);
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        let mut inode = Inode::new(Kind::Dir(BTreeMap::new()));
        inode.links = 1;
        self.inodes.insert(ino, inode);
        self.dir_entries_mut(parent)?.insert(name.clone(), ino);
        self.record(DirOpKind::Link { dir: parent, name, ino });
        Ok(())
    }

    fn readdir(&mut self, path: &str, env: &mut Env<'_>) -> Result<Vec<DirEntry>, Errno> {
        let ino = self.lookup(path)?;
        let mut out: Vec<DirEntry> = self
            .dir_entries(ino)?
            .iter()
            .map(|(n, i)| DirEntry { name: n.clone(), ino: *i, is_dir: self.inodes[i].is_dir() })
            .collect();
        // readdir(3): the order of entries is unspecified.
        if env.swarm.on(Fault::ReaddirShuffle) && out.len() > 1 {
            for i in (1..out.len()).rev() {
                let j = i - env.choices.int("kernel.readdir_swap", 0, i as u64) as usize;
                out.swap(i, j);
            }
        }
        Ok(out)
    }

    fn flock(&mut self, pid: Pid, fd: Fd, op: FlockOp, nonblock: bool) -> SysResult {
        let id = match self.ofd_of(pid, fd) {
            Ok(id) => id,
            Err(e) => return SysResult::Err(e),
        };
        let ino = self.ofds[&id].ino;
        let Some(inode) = self.inodes.get_mut(&ino) else {
            return SysResult::Err(Errno::EBADF);
        };
        // flock(2): "Converting a lock (shared to exclusive, or vice versa) is not
        // guaranteed to be atomic: the existing lock is first removed".
        let before = inode.flock.len();
        inode.flock.retain(|h| h.ofd != id);
        if inode.flock.len() != before {
            self.woken.push(WaitKey::Flock(ino));
        }
        let shared = match op {
            FlockOp::Unlock => return SysResult::Ok(SysOk::Unit),
            FlockOp::Shared => true,
            FlockOp::Exclusive => false,
        };
        let conflict = inode.flock.iter().any(|h| !(h.shared && shared));
        if conflict {
            return if nonblock { SysResult::Err(Errno::EWOULDBLOCK) } else { SysResult::Block(WaitKey::Flock(ino)) };
        }
        inode.flock.push(FlockHolder { ofd: id, shared });
        SysResult::Ok(SysOk::Unit)
    }

    fn fcntl_lock(&mut self, pid: Pid, fd: Fd, kind: Option<LockKind>, start: u64, len: u64, wait: bool) -> SysResult {
        let id = match self.ofd_of(pid, fd) {
            Ok(id) => id,
            Err(e) => return SysResult::Err(e),
        };
        let ino = self.ofds[&id].ino;
        let end = if len == 0 { u64::MAX } else { start.saturating_add(len) };
        let Some(inode) = self.inodes.get_mut(&ino) else {
            return SysResult::Err(Errno::EBADF);
        };
        match kind {
            None => {
                locks::subtract(&mut inode.fcntl, pid, start, end);
                self.woken.push(WaitKey::Fcntl(ino));
                SysResult::Ok(SysOk::Unit)
            }
            Some(k) => {
                let write = k == LockKind::Write;
                let conflict = inode
                    .fcntl
                    .iter()
                    .any(|l| l.pid != pid && l.start < end && start < l.end && (l.write || write));
                if conflict {
                    // fcntl(2): F_SETLK fails with EAGAIN; F_SETLKW waits.
                    return if wait { SysResult::Block(WaitKey::Fcntl(ino)) } else { SysResult::Err(Errno::EAGAIN) };
                }
                locks::subtract(&mut inode.fcntl, pid, start, end);
                inode.fcntl.push(FcntlLock { pid, start, end, write });
                self.woken.push(WaitKey::Fcntl(ino));
                SysResult::Ok(SysOk::Unit)
            }
        }
    }

    fn mmap(&mut self, pid: Pid, fd: Fd, offset: u64, len: usize, shared: bool) -> Result<SysOk, Errno> {
        let id = self.ofd_of(pid, fd)?;
        let ino = self.ofds[&id].ino;
        if !offset.is_multiple_of(PAGE) || len == 0 {
            return Err(Errno::EINVAL);
        }
        let reserve = self.cfg.mmap_reserve;
        let f = self.file_mut(ino)?;
        let (addr, private) = if shared {
            if offset as usize + len > reserve.max(f.cache.len()) {
                return Err(Errno::ENOMEM);
            }
            let base = f.cache.pin(reserve)?;
            // SAFETY: `offset + len` is within the pinned allocation (checked above
            // against the reserve, which `pin` rounds the capacity up to).
            (unsafe { base.as_ptr().add(offset as usize) } as usize, None)
        } else {
            // MAP_PRIVATE: copy-on-write, never written back (mmap(2)).
            let mut copy = vec![0u8; len];
            let bytes = f.cache.bytes();
            let s = (offset as usize).min(bytes.len());
            let e = (offset as usize + len).min(bytes.len());
            copy[..e - s].copy_from_slice(&bytes[s..e]);
            (0, Some(copy))
        };
        let mid = self.next_map;
        self.next_map += 1;
        let mut m = Mapping { ino, pid, offset, len, private };
        let addr = match &mut m.private {
            Some(p) => p.as_mut_ptr() as usize,
            None => addr,
        };
        self.maps.insert(mid, m);
        if let Some(i) = self.inodes.get_mut(&ino) {
            i.opens += 1;
        }
        Ok(SysOk::Map { id: mid, addr, len })
    }

    fn unmap(&mut self, id: u64) {
        if let Some(m) = self.maps.remove(&id) {
            if let Some(i) = self.inodes.get_mut(&m.ino) {
                i.opens -= 1;
            }
            self.maybe_free(m.ino);
        }
    }

    fn msync(&mut self, pid: Pid, map: u64, offset: u64, len: u64) -> Result<(), Errno> {
        let m = self.maps.get(&map).filter(|m| m.pid == pid).ok_or(Errno::EINVAL)?;
        if m.private.is_some() {
            return Ok(());
        }
        let ino = m.ino;
        let start = m.offset + offset;
        let end = (start + len.max(1)).min(m.offset + m.len as u64);
        let level = if self.cfg.profile == Profile::MacOs { Level::Dev } else { Level::Media };
        let f = self.file_mut(ino)?;
        let size = f.size();
        let pages: Vec<u64> = (start / PAGE..end.div_ceil(PAGE)).filter(|p| p * PAGE < size).collect();
        f.sync(level, Some(&pages));
        Ok(())
    }

    // ---- crashes ----

    /// Crash the machine. kill -9 only closes the process's descriptors; power
    /// loss and an OS crash kill every process and resolve every unsynced page
    /// and directory operation. Returns one line per resolved file.
    pub fn crash(&mut self, kind: CrashKind, env: &mut Env<'_>) -> Vec<String> {
        let power = match kind {
            CrashKind::Kill9(pid) => {
                self.exit(pid);
                return Vec::new();
            }
            CrashKind::PowerLoss => true,
            CrashKind::OsCrash => false,
        };
        let pids: Vec<Pid> = self.fds.keys().copied().collect();
        for pid in pids {
            self.exit(pid);
        }
        let mut lines = Vec::new();
        // Directory operations: those durable at the baseline, plus a drawn prefix
        // of the rest in program order (jbd2 and the XFS log commit metadata in
        // transaction order, so a later operation never survives an earlier one).
        let mut ns = if power { self.media_ns.clone() } else { self.dev_ns.clone() };
        let pending: Vec<&DirOp> = self.dir_ops.iter().filter(|op| if power { !op.on_media } else { !op.on_dev }).collect();
        let keep = env.choices.int("crash.dir_ops_kept", 0, pending.len() as u64) as usize;
        for op in &pending[..keep] {
            apply(&mut ns, &op.kind);
        }
        if !pending.is_empty() {
            let dropped: Vec<String> = pending[keep..].iter().map(|o| describe(&o.kind)).collect();
            let kept: Vec<String> = pending[..keep].iter().map(|o| describe(&o.kind)).collect();
            lines.push(format!("directory ops: kept {kept:?}, lost {dropped:?}"));
        }
        // Reachable inodes survive; each file resolves page by page.
        let mut reach = vec![ROOT];
        let mut seen = BTreeSet::new();
        let mut paths: BTreeMap<Ino, String> = BTreeMap::new();
        paths.insert(ROOT, "/".into());
        while let Some(d) = reach.pop() {
            if !seen.insert(d) {
                continue;
            }
            for (name, child) in ns.get(&d).cloned().unwrap_or_default() {
                let p = if d == ROOT { format!("/{name}") } else { format!("{}/{name}", paths[&d]) };
                paths.insert(child, p);
                if self.inodes.get(&child).is_some_and(|i| i.is_dir()) {
                    reach.push(child);
                } else {
                    seen.insert(child);
                }
            }
        }
        let mut inodes = BTreeMap::new();
        let aw = self.cfg.atomic_write_unit;
        for ino in &seen {
            let Some(old) = self.inodes.get(ino) else { continue };
            let kind = match &old.kind {
                Kind::Dir(_) => Kind::Dir(ns.get(ino).cloned().unwrap_or_default()),
                Kind::File(f) => {
                    let (img, summary) = f.resolve(power, env, aw);
                    if !summary.is_empty() {
                        lines.push(format!("{}: {summary}", paths.get(ino).map(String::as_str).unwrap_or("?")));
                    }
                    Kind::File(FileData::from_image(img))
                }
            };
            let mut i = Inode::new(kind);
            i.links = 1;
            inodes.insert(*ino, i);
        }
        let ns: Ns = ns.into_iter().filter(|(d, _)| seen.contains(d)).collect();
        self.inodes = inodes;
        self.media_ns = ns.clone();
        self.dev_ns = ns;
        self.dir_ops.clear();
        self.ofds.clear();
        self.maps.clear();
        self.fds.clear();
        self.woken.clear();
        lines
    }

    // ---- inspection, for oracles ----

    /// The page-cache contents of the file at `path`.
    pub fn read_file(&self, path: &str) -> Option<Vec<u8>> {
        let ino = self.lookup(path).ok()?;
        self.file(ino).ok().map(|f| f.cache.to_vec())
    }

    /// The names in the directory at `path`, sorted.
    pub fn list(&self, path: &str) -> Option<Vec<String>> {
        let ino = self.lookup(path).ok()?;
        self.dir_entries(ino).ok().map(|e| e.keys().cloned().collect())
    }

    /// Every path in the page-cache namespace, depth first, sorted.
    pub fn walk(&self) -> Vec<(String, bool)> {
        let mut out = Vec::new();
        let mut stack = vec![(ROOT, String::new())];
        while let Some((d, prefix)) = stack.pop() {
            if let Ok(e) = self.dir_entries(d) {
                for (n, i) in e.iter().rev() {
                    let p = format!("{prefix}/{n}");
                    let dir = self.inodes[i].is_dir();
                    out.push((p.clone(), dir));
                    if dir {
                        stack.push((*i, p));
                    }
                }
            }
        }
        out.sort();
        out
    }
}

fn describe(op: &DirOpKind) -> String {
    match op {
        DirOpKind::Link { name, .. } => format!("create {name}"),
        DirOpKind::Unlink { name, .. } => format!("unlink {name}"),
        DirOpKind::Rename { from, to, .. } => format!("rename {from}->{to}"),
    }
}
