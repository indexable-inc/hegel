//! POSIX signals, the small subset services use: install a handler, send a
//! signal. Under the simulation a handler runs on the receiving process's thread
//! at its next switch point, and a blocking syscall it interrupts returns
//! `EINTR` unless the handler was installed with `restart` (`SA_RESTART`).

use std::io;

pub use libc::{SIGHUP, SIGINT, SIGKILL, SIGTERM, SIGUSR1, SIGUSR2};

/// A signal handler. It runs in signal context natively, so it should only do
/// async-signal-safe things (store to an atomic).
pub type Handler = fn(i32);

mod real {
    use std::io;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static HANDLERS: [AtomicUsize; 65] = [const { AtomicUsize::new(0) }; 65];

    extern "C" fn trampoline(sig: libc::c_int) {
        if let Some(slot) = HANDLERS.get(sig as usize) {
            let p = slot.load(Ordering::Acquire);
            if p != 0 {
                // SAFETY: only `set_handler` stores here, and it stores a `fn(i32)`.
                let f: fn(i32) = unsafe { std::mem::transmute::<usize, fn(i32)>(p) };
                f(sig);
            }
        }
    }

    pub(super) fn set_handler(sig: i32, h: super::Handler, restart: bool) -> io::Result<()> {
        let slot = HANDLERS.get(sig as usize).ok_or_else(|| io::Error::from_raw_os_error(libc::EINVAL))?;
        slot.store(h as usize, Ordering::Release);
        // SAFETY: a zeroed sigaction is valid; we set the handler and flags and
        // pass valid pointers.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = trampoline as extern "C" fn(libc::c_int) as usize;
            sa.sa_flags = if restart { libc::SA_RESTART } else { 0 };
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(sig, &sa, std::ptr::null_mut()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    pub(super) fn kill(pid: u32, sig: i32) -> io::Result<()> {
        // SAFETY: kill(2) takes plain integers.
        if unsafe { libc::kill(pid as libc::pid_t, sig) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

/// Install `handler` for `sig` (`sigaction(2)`; `restart` is `SA_RESTART`).
pub fn set_handler(sig: i32, handler: Handler, restart: bool) -> io::Result<()> {
    #[cfg(feature = "sim")]
    if crate::rt::in_sim() {
        if sig == SIGKILL {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        let pid = crate::rt::current_pid();
        crate::rt::with(|w| {
            if let Some(p) = w.procs.get_mut(&pid) {
                p.handlers.insert(sig, (handler, restart));
            }
        });
        return Ok(());
    }
    real::set_handler(sig, handler, restart)
}

/// Send `sig` to process `pid` (`kill(2)`).
pub fn kill(pid: u32, sig: i32) -> io::Result<()> {
    #[cfg(feature = "sim")]
    if crate::rt::in_sim() {
        let known = crate::rt::with(|w| w.procs.contains_key(&pid));
        if !known {
            return Err(io::Error::from_raw_os_error(libc::ESRCH));
        }
        crate::rt::request(crate::rt::Req::Kill(pid, sig));
        return Ok(());
    }
    real::kill(pid, sig)
}
