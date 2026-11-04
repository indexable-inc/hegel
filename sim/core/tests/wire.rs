use hegel_sim_core::WaitKey;
use hegel_sim_core::kernel::wire::{Problem, decode_result, decode_syscall, encode_result, encode_syscall};
use hegel_sim_core::kernel::{DirEntry, Errno, FlockOp, LockKind, OpenFlags, Stat, SysOk, SysResult, Syscall, Whence};

#[test]
fn every_variant_round_trips() {
    let all = OpenFlags { read: true, write: true, create: true, excl: true, trunc: true, append: true, dsync: true, sync: true, directory: true };
    let calls = vec![
        Syscall::Open { path: "/a/b".into(), flags: all },
        Syscall::Open { path: String::new(), flags: OpenFlags { write: true, append: true, ..OpenFlags::default() } },
        Syscall::Close { fd: -1 },
        Syscall::Read { fd: 3, len: usize::MAX >> 1 },
        Syscall::Pread { fd: 4, len: 7, offset: u64::MAX },
        Syscall::Write { fd: 5, data: vec![0, 1, 255] },
        Syscall::Pwrite { fd: 6, data: Vec::new(), offset: 9 },
        Syscall::Seek { fd: 7, whence: Whence::Set(1 << 40) },
        Syscall::Seek { fd: 7, whence: Whence::Cur(-5) },
        Syscall::Seek { fd: 7, whence: Whence::End(i64::MIN) },
        Syscall::Fsync { fd: 8 },
        Syscall::Fdatasync { fd: 9 },
        Syscall::FullFsync { fd: 10 },
        Syscall::Ftruncate { fd: 11, len: 4096 },
        Syscall::Fstat { fd: 12 },
        Syscall::Stat { path: "/s".into() },
        Syscall::Rename { from: "/x".into(), to: "/y\u{e9}".into() },
        Syscall::Unlink { path: "/u".into() },
        Syscall::Mkdir { path: "/d".into() },
        Syscall::Rmdir { path: "/d".into() },
        Syscall::ReadDir { path: "/".into() },
        Syscall::Flock { fd: 13, op: FlockOp::Shared, nonblock: true },
        Syscall::Flock { fd: 13, op: FlockOp::Exclusive, nonblock: false },
        Syscall::Flock { fd: 13, op: FlockOp::Unlock, nonblock: false },
        Syscall::FcntlLock { fd: 14, kind: None, start: 0, len: 0, wait: false },
        Syscall::FcntlLock { fd: 14, kind: Some(LockKind::Read), start: 1, len: 2, wait: true },
        Syscall::FcntlLock { fd: 14, kind: Some(LockKind::Write), start: 3, len: 4, wait: false },
        Syscall::Mmap { fd: 15, offset: 8192, len: 4096, shared: true },
        Syscall::Munmap { map: 2 },
        Syscall::Msync { map: 2, offset: 10, len: 20 },
    ];
    for call in &calls {
        let mut buf = Vec::new();
        encode_syscall(call, &mut buf);
        assert_eq!(decode_syscall(&buf).as_ref(), Ok(call), "{call:?}");
    }

    let results = vec![
        SysResult::Ok(SysOk::Unit),
        SysResult::Ok(SysOk::Fd(3)),
        SysResult::Ok(SysOk::Bytes(vec![9; 300])),
        SysResult::Ok(SysOk::Count(17)),
        SysResult::Ok(SysOk::Offset(u64::MAX)),
        SysResult::Ok(SysOk::Stat(Stat { ino: 5, size: 1 << 33, is_dir: false })),
        SysResult::Ok(SysOk::Entries(vec![
            DirEntry { name: "a".into(), ino: 2, is_dir: true },
            DirEntry { name: "bb".into(), ino: 3, is_dir: false },
        ])),
        SysResult::Ok(SysOk::Entries(Vec::new())),
        SysResult::Ok(SysOk::Map { id: 1, addr: 0x1000, len: 4096 }),
        SysResult::Err(Errno::ENOSPC),
        SysResult::Block(WaitKey::Lock(1)),
        SysResult::Block(WaitKey::Cond(2)),
        SysResult::Block(WaitKey::Join(3)),
        SysResult::Block(WaitKey::Park(4)),
        SysResult::Block(WaitKey::Chan(5)),
        SysResult::Block(WaitKey::Timer(6)),
        SysResult::Block(WaitKey::Flock(7)),
        SysResult::Block(WaitKey::Fcntl(8)),
        SysResult::Block(WaitKey::Futex(9, u64::MAX)),
    ];
    for res in &results {
        let mut buf = Vec::new();
        encode_result(res, &mut buf);
        assert_eq!(decode_result(&buf).as_ref(), Ok(res), "{res:?}");
    }

    let mut buf = Vec::new();
    encode_syscall(&Syscall::Close { fd: 1 }, &mut buf);
    assert_eq!(decode_syscall(&buf[..3]).map_err(|e| (e.offset, e.problem)), Err((1, Problem::Truncated { need: 4, have: 2 })));
    buf.push(0);
    assert_eq!(decode_syscall(&buf).map_err(|e| (e.offset, e.problem)), Err((5, Problem::Trailing(1))));
    assert_eq!(decode_syscall(&[99]).map_err(|e| (e.offset, e.problem)), Err((0, Problem::UnknownTag(99))));
}
