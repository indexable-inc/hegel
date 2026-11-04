//! Linux errnos from the kernel model, as the host's `io::Error`.

use std::io;

use hegel_sim_core::Errno;

/// The host errno for a Linux errno, so `raw_os_error()` compares against the
/// host's `libc` constants as real code does.
pub(crate) fn host(e: Errno) -> i32 {
    match e {
        Errno::EPERM => libc::EPERM,
        Errno::ENOENT => libc::ENOENT,
        Errno::EINTR => libc::EINTR,
        Errno::EIO => libc::EIO,
        Errno::EBADF => libc::EBADF,
        Errno::EAGAIN => libc::EAGAIN,
        Errno::ENOMEM => libc::ENOMEM,
        Errno::EBUSY => libc::EBUSY,
        Errno::EEXIST => libc::EEXIST,
        Errno::EXDEV => libc::EXDEV,
        Errno::ENOTDIR => libc::ENOTDIR,
        Errno::EISDIR => libc::EISDIR,
        Errno::EINVAL => libc::EINVAL,
        Errno::EFBIG => libc::EFBIG,
        Errno::ENOSPC => libc::ENOSPC,
        Errno::ESPIPE => libc::ESPIPE,
        Errno::EROFS => libc::EROFS,
        Errno::ENOSYS => libc::ENOSYS,
        Errno::ENOTEMPTY => libc::ENOTEMPTY,
        Errno::EDEADLK => libc::EDEADLK,
        _ => libc::EIO,
    }
}

pub(crate) fn err(e: Errno) -> io::Error {
    io::Error::from_raw_os_error(host(e))
}
