//! A fixed binary encoding of [`Syscall`] and [`SysResult`], for crossing a
//! module boundary (a host and a simulated machine in wasm).
//!
//! Every value starts with a one-byte tag naming its variant, in declaration
//! order. Integers are little-endian and fixed width: `u64`/`i64` for offsets,
//! lengths and counts (a `usize` travels as `u64`), `i32` for descriptors and
//! errnos, `u32` for thread and process ids. A `bool` is one byte, 0 or 1.
//! Strings (UTF-8) and byte vectors are a `u32` length then the bytes.
//! [`OpenFlags`] is a `u16` bitmask in field order (`read` is bit 0,
//! `directory` bit 8). An optional [`LockKind`] is one byte: 0 unlock, 1 read,
//! 2 write. Decoding refuses unknown tags and bits, non-0/1 bools, invalid
//! UTF-8, values that do not fit and trailing bytes.

use std::fmt;

use super::{DirEntry, Errno, FlockOp, LockKind, OpenFlags, Stat, SysOk, SysResult, Syscall, Whence};
use crate::sched::WaitKey;

/// What was wrong with an encoded value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    /// The input ended `need` bytes into a field that had `have` left.
    Truncated {
        /// Bytes the field needs.
        need: u64,
        /// Bytes left in the input.
        have: usize,
    },
    /// A tag byte that names no variant.
    UnknownTag(u8),
    /// A bool byte other than 0 or 1.
    BadBool(u8),
    /// Bits set in an [`OpenFlags`] mask that name no flag.
    UnknownFlags(u16),
    /// A string that is not UTF-8.
    NotUtf8,
    /// A `u64` that does not fit this target's `usize`.
    TooLarge(u64),
    /// Bytes left over after a complete value.
    Trailing(usize),
}

/// A malformed encoding: which field, at which byte offset, and what was wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WireError {
    /// Byte offset of the field in the input.
    pub offset: usize,
    /// The field being decoded.
    pub field: &'static str,
    /// What was wrong with it.
    pub problem: Problem,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "wire: {} at byte {}: ", self.field, self.offset)?;
        match &self.problem {
            Problem::Truncated { need, have } => write!(f, "needs {need} bytes, {have} left"),
            Problem::UnknownTag(t) => write!(f, "unknown tag {t}"),
            Problem::BadBool(b) => write!(f, "bool byte {b} is neither 0 nor 1"),
            Problem::UnknownFlags(bits) => write!(f, "unknown open flag bits {bits:#06x}"),
            Problem::NotUtf8 => write!(f, "not UTF-8"),
            Problem::TooLarge(v) => write!(f, "{v} does not fit usize"),
            Problem::Trailing(n) => write!(f, "{n} trailing bytes"),
        }
    }
}

impl std::error::Error for WireError {}

const OPEN_FLAG_BITS: u16 = (1 << 9) - 1;

fn flags_bits(f: OpenFlags) -> u16 {
    [f.read, f.write, f.create, f.excl, f.trunc, f.append, f.dsync, f.sync, f.directory]
        .iter()
        .enumerate()
        .fold(0, |acc, (i, on)| acc | (u16::from(*on) << i))
}

fn flags_from_bits(b: u16) -> OpenFlags {
    let on = |i: u16| b & (1 << i) != 0;
    OpenFlags {
        read: on(0),
        write: on(1),
        create: on(2),
        excl: on(3),
        trunc: on(4),
        append: on(5),
        dsync: on(6),
        sync: on(7),
        directory: on(8),
    }
}

struct W<'a>(&'a mut Vec<u8>);

impl W<'_> {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn bool(&mut self, v: bool) {
        self.0.push(u8::from(v));
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i32(&mut self, v: i32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn usize(&mut self, v: usize) {
        self.u64(v as u64);
    }
    fn bytes(&mut self, v: &[u8]) {
        let len = u32::try_from(v.len()).unwrap_or_else(|_| panic!("wire: a {}-byte field exceeds the u32 length prefix", v.len()));
        self.u32(len);
        self.0.extend_from_slice(v);
    }
    fn str(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
    fn wait_key(&mut self, k: WaitKey) {
        match k {
            WaitKey::Lock(o) => {
                self.u8(0);
                self.u64(o);
            }
            WaitKey::Cond(o) => {
                self.u8(1);
                self.u64(o);
            }
            WaitKey::Join(t) => {
                self.u8(2);
                self.u32(t);
            }
            WaitKey::Park(t) => {
                self.u8(3);
                self.u32(t);
            }
            WaitKey::Chan(o) => {
                self.u8(4);
                self.u64(o);
            }
            WaitKey::Timer(t) => {
                self.u8(5);
                self.u32(t);
            }
            WaitKey::Flock(i) => {
                self.u8(6);
                self.u64(i);
            }
            WaitKey::Fcntl(i) => {
                self.u8(7);
                self.u64(i);
            }
            WaitKey::Futex(p, a) => {
                self.u8(8);
                self.u32(p);
                self.u64(a);
            }
        }
    }
}

struct R<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> R<'a> {
    fn err(&self, at: usize, field: &'static str, problem: Problem) -> WireError {
        WireError { offset: at, field, problem }
    }

    fn take(&mut self, field: &'static str, n: u64) -> Result<&'a [u8], WireError> {
        let have = self.buf.len() - self.pos;
        match usize::try_from(n) {
            Ok(n) if n <= have => {
                let s = &self.buf[self.pos..self.pos + n];
                self.pos += n;
                Ok(s)
            }
            _ => Err(self.err(self.pos, field, Problem::Truncated { need: n, have })),
        }
    }

    fn arr<const N: usize>(&mut self, field: &'static str) -> Result<[u8; N], WireError> {
        let s = self.take(field, N as u64)?;
        let mut a = [0u8; N];
        a.copy_from_slice(s);
        Ok(a)
    }

    fn u8(&mut self, field: &'static str) -> Result<u8, WireError> {
        Ok(self.arr::<1>(field)?[0])
    }
    fn bool(&mut self, field: &'static str) -> Result<bool, WireError> {
        let at = self.pos;
        match self.u8(field)? {
            0 => Ok(false),
            1 => Ok(true),
            b => Err(self.err(at, field, Problem::BadBool(b))),
        }
    }
    fn u16(&mut self, field: &'static str) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.arr(field)?))
    }
    fn u32(&mut self, field: &'static str) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.arr(field)?))
    }
    fn i32(&mut self, field: &'static str) -> Result<i32, WireError> {
        Ok(i32::from_le_bytes(self.arr(field)?))
    }
    fn u64(&mut self, field: &'static str) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.arr(field)?))
    }
    fn i64(&mut self, field: &'static str) -> Result<i64, WireError> {
        Ok(i64::from_le_bytes(self.arr(field)?))
    }
    fn usize(&mut self, field: &'static str) -> Result<usize, WireError> {
        let at = self.pos;
        let v = self.u64(field)?;
        usize::try_from(v).map_err(|_| self.err(at, field, Problem::TooLarge(v)))
    }
    fn bytes(&mut self, field: &'static str) -> Result<Vec<u8>, WireError> {
        let n = self.u32(field)?;
        Ok(self.take(field, u64::from(n))?.to_vec())
    }
    fn str(&mut self, field: &'static str) -> Result<String, WireError> {
        let at = self.pos;
        let b = self.bytes(field)?;
        String::from_utf8(b).map_err(|_| self.err(at, field, Problem::NotUtf8))
    }
    fn tag(&mut self, field: &'static str) -> Result<(usize, u8), WireError> {
        let at = self.pos;
        Ok((at, self.u8(field)?))
    }
    fn flags(&mut self, field: &'static str) -> Result<OpenFlags, WireError> {
        let at = self.pos;
        let b = self.u16(field)?;
        if b & !OPEN_FLAG_BITS != 0 {
            return Err(self.err(at, field, Problem::UnknownFlags(b & !OPEN_FLAG_BITS)));
        }
        Ok(flags_from_bits(b))
    }
    fn wait_key(&mut self) -> Result<WaitKey, WireError> {
        let (at, t) = self.tag("wait key tag")?;
        Ok(match t {
            0 => WaitKey::Lock(self.u64("Lock object")?),
            1 => WaitKey::Cond(self.u64("Cond object")?),
            2 => WaitKey::Join(self.u32("Join tid")?),
            3 => WaitKey::Park(self.u32("Park tid")?),
            4 => WaitKey::Chan(self.u64("Chan object")?),
            5 => WaitKey::Timer(self.u32("Timer tid")?),
            6 => WaitKey::Flock(self.u64("Flock ino")?),
            7 => WaitKey::Fcntl(self.u64("Fcntl ino")?),
            8 => WaitKey::Futex(self.u32("Futex pid")?, self.u64("Futex addr")?),
            t => return Err(self.err(at, "wait key tag", Problem::UnknownTag(t))),
        })
    }
    fn finish<T>(self, v: T) -> Result<T, WireError> {
        let left = self.buf.len() - self.pos;
        if left != 0 {
            return Err(self.err(self.pos, "end of value", Problem::Trailing(left)));
        }
        Ok(v)
    }
}

/// Append the encoding of `call` to `out`.
pub fn encode_syscall(call: &Syscall, out: &mut Vec<u8>) {
    let mut w = W(out);
    match call {
        Syscall::Open { path, flags } => {
            w.u8(0);
            w.str(path);
            w.u16(flags_bits(*flags));
        }
        Syscall::Close { fd } => {
            w.u8(1);
            w.i32(*fd);
        }
        Syscall::Read { fd, len } => {
            w.u8(2);
            w.i32(*fd);
            w.usize(*len);
        }
        Syscall::Pread { fd, len, offset } => {
            w.u8(3);
            w.i32(*fd);
            w.usize(*len);
            w.u64(*offset);
        }
        Syscall::Write { fd, data } => {
            w.u8(4);
            w.i32(*fd);
            w.bytes(data);
        }
        Syscall::Pwrite { fd, data, offset } => {
            w.u8(5);
            w.i32(*fd);
            w.bytes(data);
            w.u64(*offset);
        }
        Syscall::Seek { fd, whence } => {
            w.u8(6);
            w.i32(*fd);
            match whence {
                Whence::Set(x) => {
                    w.u8(0);
                    w.u64(*x);
                }
                Whence::Cur(d) => {
                    w.u8(1);
                    w.i64(*d);
                }
                Whence::End(d) => {
                    w.u8(2);
                    w.i64(*d);
                }
            }
        }
        Syscall::Fsync { fd } => {
            w.u8(7);
            w.i32(*fd);
        }
        Syscall::Fdatasync { fd } => {
            w.u8(8);
            w.i32(*fd);
        }
        Syscall::FullFsync { fd } => {
            w.u8(9);
            w.i32(*fd);
        }
        Syscall::Ftruncate { fd, len } => {
            w.u8(10);
            w.i32(*fd);
            w.u64(*len);
        }
        Syscall::Fstat { fd } => {
            w.u8(11);
            w.i32(*fd);
        }
        Syscall::Stat { path } => {
            w.u8(12);
            w.str(path);
        }
        Syscall::Rename { from, to } => {
            w.u8(13);
            w.str(from);
            w.str(to);
        }
        Syscall::Unlink { path } => {
            w.u8(14);
            w.str(path);
        }
        Syscall::Mkdir { path } => {
            w.u8(15);
            w.str(path);
        }
        Syscall::Rmdir { path } => {
            w.u8(16);
            w.str(path);
        }
        Syscall::ReadDir { path } => {
            w.u8(17);
            w.str(path);
        }
        Syscall::Flock { fd, op, nonblock } => {
            w.u8(18);
            w.i32(*fd);
            w.u8(match op {
                FlockOp::Shared => 0,
                FlockOp::Exclusive => 1,
                FlockOp::Unlock => 2,
            });
            w.bool(*nonblock);
        }
        Syscall::FcntlLock { fd, kind, start, len, wait } => {
            w.u8(19);
            w.i32(*fd);
            w.u8(match kind {
                None => 0,
                Some(LockKind::Read) => 1,
                Some(LockKind::Write) => 2,
            });
            w.u64(*start);
            w.u64(*len);
            w.bool(*wait);
        }
        Syscall::Mmap { fd, offset, len, shared } => {
            w.u8(20);
            w.i32(*fd);
            w.u64(*offset);
            w.usize(*len);
            w.bool(*shared);
        }
        Syscall::Munmap { map } => {
            w.u8(21);
            w.u64(*map);
        }
        Syscall::Msync { map, offset, len } => {
            w.u8(22);
            w.u64(*map);
            w.u64(*offset);
            w.u64(*len);
        }
    }
}

/// Decode one [`Syscall`] that fills all of `buf`.
pub fn decode_syscall(buf: &[u8]) -> Result<Syscall, WireError> {
    let mut r = R { buf, pos: 0 };
    let (at, tag) = r.tag("syscall tag")?;
    let call = match tag {
        0 => Syscall::Open { path: r.str("open path")?, flags: r.flags("open flags")? },
        1 => Syscall::Close { fd: r.i32("close fd")? },
        2 => Syscall::Read { fd: r.i32("read fd")?, len: r.usize("read len")? },
        3 => Syscall::Pread { fd: r.i32("pread fd")?, len: r.usize("pread len")?, offset: r.u64("pread offset")? },
        4 => Syscall::Write { fd: r.i32("write fd")?, data: r.bytes("write data")? },
        5 => Syscall::Pwrite { fd: r.i32("pwrite fd")?, data: r.bytes("pwrite data")?, offset: r.u64("pwrite offset")? },
        6 => {
            let fd = r.i32("lseek fd")?;
            let (wat, w) = r.tag("lseek whence tag")?;
            let whence = match w {
                0 => Whence::Set(r.u64("SEEK_SET offset")?),
                1 => Whence::Cur(r.i64("SEEK_CUR delta")?),
                2 => Whence::End(r.i64("SEEK_END delta")?),
                t => return Err(r.err(wat, "lseek whence tag", Problem::UnknownTag(t))),
            };
            Syscall::Seek { fd, whence }
        }
        7 => Syscall::Fsync { fd: r.i32("fsync fd")? },
        8 => Syscall::Fdatasync { fd: r.i32("fdatasync fd")? },
        9 => Syscall::FullFsync { fd: r.i32("F_FULLFSYNC fd")? },
        10 => Syscall::Ftruncate { fd: r.i32("ftruncate fd")?, len: r.u64("ftruncate len")? },
        11 => Syscall::Fstat { fd: r.i32("fstat fd")? },
        12 => Syscall::Stat { path: r.str("stat path")? },
        13 => Syscall::Rename { from: r.str("rename from")?, to: r.str("rename to")? },
        14 => Syscall::Unlink { path: r.str("unlink path")? },
        15 => Syscall::Mkdir { path: r.str("mkdir path")? },
        16 => Syscall::Rmdir { path: r.str("rmdir path")? },
        17 => Syscall::ReadDir { path: r.str("readdir path")? },
        18 => {
            let fd = r.i32("flock fd")?;
            let (oat, o) = r.tag("flock op")?;
            let op = match o {
                0 => FlockOp::Shared,
                1 => FlockOp::Exclusive,
                2 => FlockOp::Unlock,
                t => return Err(r.err(oat, "flock op", Problem::UnknownTag(t))),
            };
            Syscall::Flock { fd, op, nonblock: r.bool("flock nonblock")? }
        }
        19 => {
            let fd = r.i32("fcntl_lock fd")?;
            let (kat, k) = r.tag("fcntl_lock kind")?;
            let kind = match k {
                0 => None,
                1 => Some(LockKind::Read),
                2 => Some(LockKind::Write),
                t => return Err(r.err(kat, "fcntl_lock kind", Problem::UnknownTag(t))),
            };
            Syscall::FcntlLock {
                fd,
                kind,
                start: r.u64("fcntl_lock start")?,
                len: r.u64("fcntl_lock len")?,
                wait: r.bool("fcntl_lock wait")?,
            }
        }
        20 => Syscall::Mmap {
            fd: r.i32("mmap fd")?,
            offset: r.u64("mmap offset")?,
            len: r.usize("mmap len")?,
            shared: r.bool("mmap shared")?,
        },
        21 => Syscall::Munmap { map: r.u64("munmap map")? },
        22 => Syscall::Msync { map: r.u64("msync map")?, offset: r.u64("msync offset")?, len: r.u64("msync len")? },
        t => return Err(r.err(at, "syscall tag", Problem::UnknownTag(t))),
    };
    r.finish(call)
}

/// Append the encoding of `result` to `out`.
pub fn encode_result(result: &SysResult, out: &mut Vec<u8>) {
    let mut w = W(out);
    match result {
        SysResult::Ok(v) => {
            w.u8(0);
            match v {
                SysOk::Unit => w.u8(0),
                SysOk::Fd(fd) => {
                    w.u8(1);
                    w.i32(*fd);
                }
                SysOk::Bytes(b) => {
                    w.u8(2);
                    w.bytes(b);
                }
                SysOk::Count(n) => {
                    w.u8(3);
                    w.usize(*n);
                }
                SysOk::Offset(o) => {
                    w.u8(4);
                    w.u64(*o);
                }
                SysOk::Stat(s) => {
                    w.u8(5);
                    w.u64(s.ino);
                    w.u64(s.size);
                    w.bool(s.is_dir);
                }
                SysOk::Entries(es) => {
                    w.u8(6);
                    let n = u32::try_from(es.len()).unwrap_or_else(|_| panic!("wire: {} directory entries exceed the u32 count", es.len()));
                    w.u32(n);
                    for e in es {
                        w.str(&e.name);
                        w.u64(e.ino);
                        w.bool(e.is_dir);
                    }
                }
                SysOk::Map { id, addr, len } => {
                    w.u8(7);
                    w.u64(*id);
                    w.usize(*addr);
                    w.usize(*len);
                }
            }
        }
        SysResult::Err(e) => {
            w.u8(1);
            w.i32(e.0);
        }
        SysResult::Block(k) => {
            w.u8(2);
            w.wait_key(*k);
        }
    }
}

/// Decode one [`SysResult`] that fills all of `buf`.
pub fn decode_result(buf: &[u8]) -> Result<SysResult, WireError> {
    let mut r = R { buf, pos: 0 };
    let (at, tag) = r.tag("result tag")?;
    let res = match tag {
        0 => {
            let (oat, o) = r.tag("ok tag")?;
            SysResult::Ok(match o {
                0 => SysOk::Unit,
                1 => SysOk::Fd(r.i32("ok fd")?),
                2 => SysOk::Bytes(r.bytes("ok bytes")?),
                3 => SysOk::Count(r.usize("ok count")?),
                4 => SysOk::Offset(r.u64("ok offset")?),
                5 => SysOk::Stat(Stat { ino: r.u64("stat ino")?, size: r.u64("stat size")?, is_dir: r.bool("stat is_dir")? }),
                6 => {
                    let n = r.u32("entry count")?;
                    let mut es = Vec::new();
                    for _ in 0..n {
                        es.push(DirEntry { name: r.str("entry name")?, ino: r.u64("entry ino")?, is_dir: r.bool("entry is_dir")? });
                    }
                    SysOk::Entries(es)
                }
                7 => SysOk::Map { id: r.u64("map id")?, addr: r.usize("map addr")?, len: r.usize("map len")? },
                t => return Err(r.err(oat, "ok tag", Problem::UnknownTag(t))),
            })
        }
        1 => SysResult::Err(Errno(r.i32("errno")?)),
        2 => SysResult::Block(r.wait_key()?),
        t => return Err(r.err(at, "result tag", Problem::UnknownTag(t))),
    };
    r.finish(res)
}
