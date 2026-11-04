//! `std::sync`, with atomics, locks, condition variables and channels as switch
//! points under the simulation.

#[cfg(not(feature = "sim"))]
pub use std::sync::{Barrier, Condvar, LazyLock, Mutex, MutexGuard, Once, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard, WaitTimeoutResult, mpsc};
pub use std::sync::{Arc, LockResult, PoisonError, TryLockError, TryLockResult, Weak};

pub mod atomic;

#[cfg(feature = "sim")]
mod locks;
#[cfg(feature = "sim")]
pub mod mpsc;
#[cfg(feature = "sim")]
pub use locks::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard, WaitTimeoutResult};
#[cfg(feature = "sim")]
pub use std::sync::{LazyLock, Once, OnceLock};
