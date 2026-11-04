//! `std::process`, with `exit` and `id` simulated.

#[cfg(not(feature = "sim"))]
pub use std::process::*;

#[cfg(feature = "sim")]
pub use std::process::{ExitCode, ExitStatus, Termination, abort};

/// `std::process::exit`: under the simulation, the simulated process exits
/// (other threads are killed, no destructors run, descriptors close).
#[cfg(feature = "sim")]
pub fn exit(code: i32) -> ! {
    if crate::rt::in_sim() {
        crate::rt::request(crate::rt::Req::ExitProcess(code));
        unreachable!("hegel-sim: a process ran after exit");
    }
    std::process::exit(code)
}

/// `std::process::id`: the simulated pid under the simulation.
#[cfg(feature = "sim")]
pub fn id() -> u32 {
    if crate::rt::in_sim() { crate::rt::current_pid() } else { std::process::id() }
}
