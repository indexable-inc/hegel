//! flock and fcntl record lock state.

use crate::Pid;

/// One flock holder: an open file description (flock(2): locks belong to it).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FlockHolder {
    pub(crate) ofd: u64,
    pub(crate) shared: bool,
}

/// One fcntl record lock: owned by a process over `[start, end)`.
#[derive(Clone, Copy, Debug)]
pub(crate) struct FcntlLock {
    pub(crate) pid: Pid,
    pub(crate) start: u64,
    pub(crate) end: u64,
    pub(crate) write: bool,
}

/// Remove `pid`'s locks over `[start, end)`, splitting ranges that straddle it
/// (fcntl(2): unlocking part of a range splits it).
pub(crate) fn subtract(locks: &mut Vec<FcntlLock>, pid: Pid, start: u64, end: u64) {
    let mut out = Vec::with_capacity(locks.len());
    for l in locks.drain(..) {
        if l.pid != pid || l.end <= start || end <= l.start {
            out.push(l);
            continue;
        }
        if l.start < start {
            out.push(FcntlLock { end: start, ..l });
        }
        if end < l.end {
            out.push(FcntlLock { start: end, ..l });
        }
    }
    *locks = out;
}
