//! The sans-IO core of Hegel's deterministic simulation.
//!
//! Everything here is a synchronous state machine: the Linux kernel model
//! ([`kernel`]), the PCT scheduler ([`sched`]), the virtual clock ([`clock`]),
//! the fault classes and swarm ([`faults`]) and the [`trace`]. Nothing owns a
//! clock, randomness or I/O. Every nondeterministic choice is a draw through the
//! [`Choices`] port, which Hegel answers in a test (so failing runs shrink) and
//! [`SeededChoices`] answers elsewhere. The native driver (`hegel-sim`) and the
//! WASI host (`hegel-wasi`) both drive this core. See `docs/dst.md`.

pub mod choices;
pub mod clock;
pub mod faults;
pub mod kernel;
pub mod sched;
pub mod trace;

pub use choices::{Choices, SeededChoices};
pub use clock::Clock;
pub use faults::{Fault, FaultSet, Rates, Swarm};
pub use kernel::{Errno, Kernel, Profile, SysOk, SysResult, Syscall};
pub use sched::{Pick, Sched, Tid, WaitKey};
pub use trace::Trace;

/// A simulated process id.
pub type Pid = u32;
