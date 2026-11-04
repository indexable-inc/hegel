//! Native snapshots by `fork(2)`, AFL fork-server style: run a [`Sim`] to a
//! state once, then explore many continuations from it, each in a forked child
//! that inherits the whole simulated world copy-on-write.
//!
//! Limits (the wasm host's snapshot manager has none of these):
//! - A snapshot cannot be saved, deduplicated or shipped; it exists only as the
//!   parent process's memory.
//! - One child process per branch, so branches cost a fork each (fast, but not
//!   free) and cannot be forked again from their own midpoints here.
//! - Only the calling thread exists in a child. Fork while other host threads
//!   might hold a lock the child needs (the allocator's, stdout's) and the child
//!   can deadlock; run such tests single-threaded (`--test-threads=1`).
//! - On macOS, frameworks that refuse to run after fork (Objective-C runtime
//!   initialisation, CoreFoundation) must not be touched in a child.
//! - A branch answers its choices from a seed ([`SeededChoices`]), never from the
//!   parent's test case, so branches do not shrink; [`replay`] re-runs one
//!   branch's suffix in-process from its seed.

use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::panic::{AssertUnwindSafe, catch_unwind};

use hegel_sim_core::SeededChoices;

use crate::sim::Sim;

/// One explored continuation.
#[derive(Clone, Debug)]
pub struct Branch {
    /// Its index, `0..n`.
    pub index: u32,
    /// The seed its choices came from.
    pub seed: u64,
    /// What the body returned, or the panic message (with the trace) if it failed.
    pub outcome: Result<String, String>,
}

/// The seed branch `index` of an exploration with `base_seed` uses.
pub fn branch_seed(base_seed: u64, index: u32) -> u64 {
    base_seed ^ (u64::from(index).wrapping_add(1)).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

fn run_branch(sim: &mut Sim<'_>, seed: u64, body: &dyn Fn(&mut Sim<'_>) -> String) -> Result<String, String> {
    let choices: &'static SeededChoices = Box::leak(Box::new(SeededChoices::new(seed))); // alloc: lives as long as the branch
    // SAFETY: `choices` is leaked, so it outlives the simulation.
    unsafe { sim.set_choices(choices) };
    catch_unwind(AssertUnwindSafe(|| body(sim))).map_err(|p| {
        let msg = if let Some(s) = p.downcast_ref::<&str>() {
            (*s).to_owned()
        } else if let Some(s) = p.downcast_ref::<String>() {
            s.clone()
        } else {
            "(non-string panic payload)".to_owned()
        };
        format!("{msg}\n{}", sim.trace())
    })
}

/// Explore `n` continuations of `sim`'s current state: each forked child runs
/// `body` on its copy of the world with its own seeded choices and reports what
/// happened. The parent's `sim` is untouched.
pub fn explore(sim: &mut Sim<'_>, n: u32, base_seed: u64, body: impl Fn(&mut Sim<'_>) -> String) -> std::io::Result<Vec<Branch>> {
    let mut children = Vec::with_capacity(n as usize); // alloc: one entry per branch
    for index in 0..n {
        let seed = branch_seed(base_seed, index);
        let mut fds = [0; 2];
        // SAFETY: pipe(2) writes two descriptors into `fds`.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: fork(2); the child only runs the simulation and then `_exit`s.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(std::io::Error::last_os_error());
        }
        if pid == 0 {
            // SAFETY: the child owns these descriptors.
            unsafe { libc::close(fds[0]) };
            let out = run_branch(sim, seed, &body);
            let mut msg = Vec::new();
            match out {
                Ok(s) => {
                    msg.push(b'O');
                    msg.extend_from_slice(s.as_bytes());
                }
                Err(s) => {
                    msg.push(b'E');
                    msg.extend_from_slice(s.as_bytes());
                }
            }
            // SAFETY: the write end belongs to the child.
            let mut w = unsafe { std::fs::File::from_raw_fd(fds[1]) };
            let _ = w.write_all(&msg);
            drop(w);
            // SAFETY: leave without running the parent's atexit handlers or
            // destructors, which belong to the parent.
            unsafe { libc::_exit(0) };
        }
        // SAFETY: the parent owns the read end and closes its copy of the write end.
        unsafe { libc::close(fds[1]) };
        // SAFETY: as above.
        children.push((index, seed, pid, unsafe { std::fs::File::from_raw_fd(fds[0]) }));
    }
    let mut out = Vec::with_capacity(children.len()); // alloc: one entry per branch
    for (index, seed, pid, mut r) in children {
        let mut buf = Vec::new();
        r.read_to_end(&mut buf)?;
        let mut status = 0;
        // SAFETY: waits on our own child.
        unsafe { libc::waitpid(pid, &mut status, 0) };
        let text = String::from_utf8_lossy(buf.get(1..).unwrap_or_default()).into_owned();
        let outcome = match buf.first() {
            Some(b'O') => Ok(text),
            Some(b'E') => Err(text),
            _ => Err(format!("hegel-sim: branch {index} died without reporting (wait status {status})")),
        };
        out.push(Branch { index, seed, outcome });
    }
    Ok(out)
}

/// Re-run one branch's continuation in this process, consuming `sim`'s state:
/// the way to debug a failing [`Branch`] (a panic propagates with the trace).
pub fn replay(sim: &mut Sim<'_>, seed: u64, body: impl Fn(&mut Sim<'_>) -> String) -> String {
    let choices: &'static SeededChoices = Box::leak(Box::new(SeededChoices::new(seed))); // alloc: lives as long as the run
    // SAFETY: leaked, so it outlives the simulation.
    unsafe { sim.set_choices(choices) };
    body(sim)
}
