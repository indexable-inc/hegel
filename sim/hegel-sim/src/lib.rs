//! Deterministic simulation for Hegel.
//!
//! A facade over `std::sync`, `std::thread`, `std::time` and `std::fs`, plus
//! `mmap` and `signal`. Without the `sim` feature every item is the std item (or a
//! thin libc call), so production code pays nothing. With `sim`, every item is a
//! wrapper that, inside a running [`Sim`], is a switch point for the PCT
//! scheduler and routes I/O to the simulated Linux kernel; outside a running
//! simulation it behaves exactly like std. Opt a crate in by importing from here
//! instead of std and banning the std items with `clippy-facade.toml`.
//!
//! See `docs/dst.md` in the Hegel repository.

#[cfg(feature = "sim")]
mod errno;
pub mod fs;
pub mod hint;
pub mod local;
pub mod mmap;
pub mod oracle;
pub mod process;
pub mod signal;
pub mod sync;
pub mod thread;
pub mod time;

#[cfg(feature = "sim")]
mod rt;
#[cfg(feature = "sim")]
mod sim;
#[cfg(feature = "sim")]
pub mod fork;

#[cfg(feature = "sim")]
pub use hegel_sim_core::kernel::{CrashKind, KernelConfig, Profile};
#[cfg(feature = "sim")]
pub use hegel_sim_core::{Choices, Fault, FaultSet, Pid, Rates, SeededChoices};
#[cfg(feature = "sim")]
pub use sim::{Config, CrashPoints, Exit, Outcome, Sim};
#[cfg(feature = "sim")]
pub use hegel_sim_core as core;
