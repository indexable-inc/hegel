//! Hegel's simulated worlds in wasm. The machine (kernel model, scheduler,
//! clock) is one wasm module; every process is one instance of its program;
//! the host only routes calls, answers draws and keeps small tables. A world
//! can be snapshot, forked, diffed, saved and shipped.

pub mod instrument;
mod machine;
pub mod snapshot;
mod world;

pub use snapshot::{PAGE, PageStore, Snapshot};
pub use world::{MachineModule, Outcome, Program, Programs, StackMode, WasmConfig, World, engine};
