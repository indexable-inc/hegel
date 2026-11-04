---
status: plan
phases:
  phase-1-native: landed
  phase-2a-wasm-asyncify: building
  phase-2b-wasm-stack-switching: planned
  phase-3-validation: written, not run
---

# Deterministic simulation testing in Hegel

Status: **plan**. The status of each phase is in the front matter above and in "Phases" at the end, and is
updated as phases land. Where the code and this document disagree, the code wins and this document is the bug.

## Goal

One Hegel-native harness for concurrent, crash-surviving code. Inside it a program's threads, its syscalls, its
clock and its machine's power are all simulated, and every choice the simulation makes (which thread runs next,
which unsynced page survives a power cut, whether this `write` gets `ENOSPC`) is a Hegel draw. A failing case
therefore shrinks like any other Hegel case: to the fewest preemptions, faults and crashes that still fail. It
replays from a seed or a failure blob, and prints the schedule and fault list that cause the failure.

A test written with it must find these bug shapes by itself, without hints about where the bug is. Each one
comes from a real store: a crash-surviving append log with a JSON manifest as its commit point, Parquet files
renamed into place, and per-thread lock-free rings in mmap'd files:

| # | Bug shape | What the harness must do to find it |
|---|---|---|
| 1 | No `fsync` before `rename`. Power loss tears `manifest.json`, or leaves it listing a file whose bytes never reached disk, after the inputs were deleted. | Page cache with durability only through fsync; power loss that loses any subset of unsynced pages and keeps a journal-order prefix of the directory operations. |
| 2 | Crash between the manifest commit and marking rows consumed: rows are counted twice on recovery. | kill -9 at any syscall, then a restart that runs recovery on the surviving disk. |
| 3 | Partial flush failure (`ENOSPC` on one table, a lock timeout on commit) leading to duplicates or loss. | `ENOSPC` at `write` and at `fsync`; lock waits with timeouts on the virtual clock. |
| 4 | An old binary and a new binary sharing one on-disk state (version skew). | Restart that draws which program version runs next. |
| 5 | A lock held by one process across a long recovery makes another process's startup fail. | `flock` between simulated processes; virtual time so a "long" recovery is long in simulated seconds and costs nothing in real time. |
| 6 | Lock-free lost update: thread A copies a store, including its `flushed` counter, into a new mmap'd store and swaps an `AtomicPtr`. Meanwhile thread B writes `flushed` through the old pointer. The update is lost, and a crash then re-emits rows. | A switch point at **every atomic operation**, because no lock, spawn or channel lies between those atomics. Also needed: mmap `MAP_SHARED` memory that survives kill -9. |

Shape 6 is why a cooperative scheduler is not enough. Shuttle, `det-sim`'s async runtime, madsim and turmoil
switch only at spawn, lock, channel and await points. Loom and this harness switch at every atomic operation.

## Prior art and what was taken

- **ix `det-sim`**: an ALICE-style crash model (persist units, file and directory fsync as independent barriers,
  a `Chooser` port) and a deterministic async runtime with virtual time and deadlock detection. Taken: the
  `Chooser`-as-port shape, deadlock as a typed outcome, and "a crash exposes any legal subset of what is not
  barriered". Not taken: recording ops for replay. Here the kernel model is live, and every program syscall
  goes through it.
- **ix `weakmem`**: a facade that is std in a normal build and the model under a feature. It covers loom's
  weak-memory model with vector clocks, and every load choice is a draw. Taken: the facade shape (std types in
  normal builds, a feature that switches them, a lint that bans raw std atomics). weakmem stays the answer for
  weak memory (see "Weak memory").
- **ix `storage-sim`**: a disk model with a volatile write cache, flush and FUA, and crash fates of lost, whole,
  or torn at a sector boundary. Taken: tearing at the device's atomic write unit, and FUA (`O_DSYNC`) as
  durable at completion.
- **ix `dst-kit` and `hegel-explore`**, and ix `docs/design/hegel-coverage.md`: swarm testing, where each run
  enables a drawn subset of fault classes and choice 0 stays the calm choice. Its sections "Faults follow Linux
  semantics" and "Wasm targets" are the direction this document builds out.
- **blaze `sim.rs`**: a seeded driver whose crash keeps a random prefix of an unsynced append (a torn append).
  This is generalised here to per-page fates.
- **shuttle**: PCT (Burckhardt et al., ASPLOS'10) as a scheduler over a facade of std types. Its `yield_now`
  deprioritises the caller under PCT. Taken: the PCT algorithm and the yield rule. Not taken: OS threads with a
  baton. Fibers on one OS thread are used here instead.
- **loom**: a switch point at every atomic operation, and exhaustive DPOR. This harness samples instead of
  enumerating, because it runs whole programs with crashes and I/O. Loom's state space is impractical there.
- **FoundationDB simulation** (BUGGIFY): fault injection at real code sites, enabled per run. Swarm here is the
  same idea per fault class.
- **TigerBeetle VOPR**: a storage fault model with a seed per run and "liveness mode" after faults stop. Taken:
  a final calm epoch after the crash budget is spent, in which recovery must complete.
- **CrashMonkey, ALICE, dm-log-writes**: recording block I/O and replaying crash states. Phase 3 uses them to
  check this kernel model against real kernels.
- **wasmfuzz**: load-time instrumentation of a wasm module. Phase 2 uses the same technique to insert a
  scheduler yield before every atomic instruction.

## Crate layout

All new crates are in the workspace under `sim/`, MIT, with no ix-specific code.

| Crate | Path | What it is |
|---|---|---|
| `hegel-sim-core` | `sim/core` | Sans-IO and dependency-free. The kernel model (`kernel`), the PCT scheduler core (`sched`), the virtual clock (`clock`), fault classes and swarm (`faults`), the `Choices` port, and the trace. Nothing in it does I/O, owns randomness or reads a clock. Native and WASI drivers share it. |
| `hegel-sim` | `sim/hegel-sim` | The facade (`sync`, `thread`, `time`, `fs`, `mmap`, `signal`, `process`, `hint`) and the native driver (`Sim`, fibers via corosensei). Without the `sim` feature, every facade item is the std type or function, re-exported. With `sim`, each is a thin wrapper that runs a switch point before the std operation when called inside a running simulation, and behaves exactly like std outside one. |
| `hegeltest` (feature `sim`) | `frontends/rust` | `impl hegel_sim::Choices for TestCase` and `pub use hegel_sim as sim`, so a test writes `hegel::sim::Sim::new(&tc, ..)`. |
| `hegel-sim-machine` | `sim/machine` | Phase 2: `hegel-sim-core` behind a C ABI, built as a `wasm32-unknown-unknown` cdylib: one instance is one simulated machine (kernel, scheduler, clock, swarm, trace), all of it in that instance's linear memory. Its only imports are the draws. |
| `hegel-sim-wasm` | `sim/wasm-host` | Phase 2: the Wasmtime host. It instruments and loads program modules, routes their WASI imports to the machine instance, switches their threads, and owns the snapshot manager. |
| `hegel-sim-examples` | `sim/examples` | `publish = false`. The six planted bugs as tiny programs, each with a buggy and a fixed variant, and the Hegel tests that catch the buggy variant and pass the fixed one. |
| validation | `sim/validation` | Phase 3: `validate_kernel_model.py` and the model's crash-state membership checker. |

The kernel model must compile to both native code and `wasm32-unknown-unknown` from phase 1 on, because phase 2
runs it as a wasm module; it has no dependencies, no I/O and no `std::time`, and CI builds it for both
targets.

Production crates depend on `hegel-sim` only, which in a normal build has no dependencies and compiles to
re-exports of std. A test enables `hegel-sim/sim`. Cargo's feature unification then also turns the production
crate's facade into the simulated one in that test build. This is safe, because outside a running simulation
each wrapper is the std operation.

### Opting a crate in, and the lint

A crate opts in by importing from the facade instead of std:

```rust
use hegel_sim::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use hegel_sim::sync::{Mutex, Condvar};
use hegel_sim::{fs, thread, time::Instant};
```

Raw std items are then banned in that crate with clippy's `disallowed-types` and `disallowed-methods`. A
ready-made `clippy.toml` fragment lives at `sim/hegel-sim/clippy-facade.toml`. It covers
`std::sync::atomic::Atomic*`, `std::sync::{Mutex, RwLock, Condvar}`, `std::thread::{spawn, sleep}`,
`std::time::{Instant, SystemTime}`, `std::fs::*` and `std::hint::spin_loop`, and names the facade path as
the replacement in each `reason`. The facade crate itself is the only place allowed to name the std items.

The facade atomics are `#[repr(transparent)]` over the std atomics, so a pointer into shared memory (an mmap'd
ring header) can be cast to `&hegel_sim::sync::atomic::AtomicU64` exactly as to the std type.

## The scheduler

### Fibers

Every simulated thread of every simulated process is a corosensei coroutine on the one OS thread running the
Hegel test case. Each has its own stack (default 256 KiB, `Config::stack_size`). The driver loop lives on the
test's own stack. A fiber suspends at every switch point and hands the driver a request: an operation about to
happen, a syscall to run, a wait to block on, or an exit. The driver applies it to the sans-IO core, asks the
scheduler which thread runs next, and resumes that thread. Because syscalls run on the driver's stack, every
fault draw and every kernel state change happens on the driver's stack and never on a fiber's.

Switch points:

- every atomic operation (load, store, swap, every RMW, compare-exchange, fence), *before* it takes effect;
- `Mutex::lock`/`try_lock` and guard drop, `Condvar::wait*`/`notify*`, `RwLock`;
- `thread::spawn`, `JoinHandle::join`, `thread::park`/`unpark`, `thread::yield_now`, `thread::sleep`;
- `mpsc` send and receive;
- every syscall (`fs`, `mmap`/`msync`, `flock`, signals, the clock);
- `hint::spin_loop`.

A switch costs one stack switch (tens of nanoseconds). A case with 10,000 atomics costs well under a millisecond
of switching.

### PCT with every parameter drawn

The scheduler is PCT (Burckhardt, Kothari, Musuvathi, Nagarakatte, ASPLOS'10). It finds a bug of depth `d`
with probability at least `1/(n·k^(d-1))` per run, where `n` is the number of threads and `k` the number of
steps. Depth 1 is an ordering bug, and depth 2 is "preempt A here, let B run". Every parameter is a Hegel
draw, made once and up front, so a case costs a handful of draws and not one per step:

1. **Priority per thread**, drawn at spawn: `integers(0, 2^16)`. The highest runnable `(priority, -spawn_ordinal)`
   runs. The all-zero tape gives "the oldest runnable thread runs until it blocks", the calm schedule.
2. **Depth** `d ∈ [0, Config::max_depth]`, default 3. It shrinks to 0, meaning no preemption.
3. **Horizon exponent** `e ∈ [4, 17]`. The horizon is `2^e` steps. No test author has to know how many steps a
   program takes: about a third of the cases land within a factor of 2 of any program's real length. With
   `Config::steps_hint(n)` the horizon is fixed at `n` and `e` is not drawn.
4. **Change points**: `d-1`, plus one if `d > 0`, step indices drawn uniformly in `[0, 2^e)` and sorted. When the
   global step counter reaches change point `i`, the running thread's priority drops below every initial
   priority (`-(i+1)`, as in PCT). The scheduler then switches at the very next switch point, which is the
   preemption.

Rules on top of PCT:

- **Yield and spin.** `thread::yield_now`, `hint::spin_loop`, and the same atomic load returning the same value
  32 times in a row on one thread all demote that thread to the lowest priority until another thread takes a
  step. Without this rule, a high-priority spinner waiting on a low-priority thread livelocks (shuttle's PCT has
  the same rule).
- **Blocked threads** (a lock, join, condvar, park, sleep, flock or channel wait) are not runnable. When nothing
  is runnable, the clock jumps to the earliest timer. When there is no timer either, the run ends in
  `Outcome::Deadlock`, naming every blocked thread and what it waits for.
- **Bounded.** `Config::max_steps` (default 200,000) ends the run in `Outcome::StepBound`, naming the thread
  that ran last and its operation. Every wait the driver performs is bounded.
- **Random-walk mode** is one swarm class (`RandomPreemption`). When it is enabled, each switch point also
  draws a weighted coin (p = 1/64). On true, a uniformly drawn runnable thread runs. This costs one draw per
  step, and it finds bugs whose depth exceeds `max_depth`.

### Processes, kill -9, power loss, restart

A simulated **process** is a group of fibers plus the kernel's fd table and locks for that pid. Its program is a
`'static` closure `FnOnce() -> i32`. The process exits when its main thread returns or calls
`process::exit`. Other processes are not affected by the exit.

- **kill -9** of a process: every fiber of it is reset in place with corosensei's `force_reset`. No destructor
  runs, as with a real SIGKILL: a `BufWriter` is not flushed and a guard does not unlock. The kernel then
  closes the process's fds, releases its `flock` and `fcntl` locks, and unmaps its mappings. Its page cache
  writes survive, including `MAP_SHARED` stores. Safety: nothing outside the process holds a pointer into its
  fiber stacks. The kernel stores no user pointers, scheduler wait queues hold thread ids, and programs are
  `'static`. Leaked heap objects are harmless.
- **Power loss**: kill -9 of every process, then the kernel resolves every unsynced page and directory operation
  (see "Kernel model").
- **OS crash** (kernel panic): like power loss, but the device's volatile cache survives. This only differs
  from power loss under the macOS profile, where `fsync` stops at the device cache.
- **Crash points.** A crash can fire at any syscall, or at any switch point under
  `Config::crash_points(CrashPoints::EverySwitch)`. At each crash point, while the crash budget
  (`Config::max_crashes`, default 2) is not spent and the run's crash classes allow it, the driver draws a
  weighted coin. Its probability is drawn once per epoch, log-uniform from 1/2 to 1/2048, so short and long
  programs both crash at a useful rate without a knob. The crash kind (kill -9 of a drawn process, power loss,
  or OS crash) is drawn from the classes the swarm enabled.
- **Restart**: `Sim::run` returns `Outcome::Crashed`, and the test spawns the next processes on the surviving
  disk. `Sim::pick(&[..])` draws which program version (for example v1 or v2) runs next, shrinking toward the
  first. After the budget is spent, one more epoch runs with no crash, as in VOPR's liveness mode, so recovery
  must finish.

Rust state shared between processes is outside the model, with one sanctioned exception: **ghost state** for the
oracle (`Rc<RefCell<_>>` captured by programs). It records what the test may assert, such as rows acknowledged
durable. It lives outside the simulated machine and survives every crash. Programs must not use it to
communicate.

### Signals

`signal::set_handler(sig, handler, flags)` registers a handler in the kernel's disposition table for the
current process. `signal::kill(pid, sig)` raises a signal, as does a drawn asynchronous `SIGTERM` (swarm class
`AsyncSignals`). Delivery happens at the next switch point of a thread of the target process, chosen by the
scheduler's next pick among that process's threads. The handler runs on that thread's fiber between two
operations, so it can land between any two atomics. The signal is blocked while its handler runs (no
`SA_NODEFER`). A thread blocked in an interruptible wait (sleep, a blocking `flock`) when a handled signal
arrives returns `EINTR`. With `SA_RESTART` the restartable calls restart instead, per signal(7) "Interruption of
system calls and library functions by signal handlers". `nanosleep` is never restarted, and the facade's
`thread::sleep` retries as std does. With no handler, `SIGTERM`, `SIGINT` and `SIGKILL` terminate the process
as kill -9 does.

### Thread locals

`std::thread_local!` is one slot per OS thread, so under the sim every simulated thread would share it, a
semantic difference the harness cannot see. The facade's `hegel_sim::thread_local!` keys its slots by simulated
thread inside a simulation and is std's macro otherwise, and the lint bans `std::thread_local!` in opted-in
crates.

## Kernel model

`hegel_sim_core::kernel::Kernel` is a synchronous state machine: `syscall(pid, Syscall, &mut dyn Choices) ->
SysResult`, where a call that must wait returns `SysResult::Block(WaitKey)` and is retried when the key is
woken. `crash(kind, &mut dyn Choices) -> CrashReport`, `exit(pid)`. It owns no clock, no randomness and no I/O.
Every fault is a draw through the `Choices` port, and only for fault classes the run's swarm enabled. Each rule
below is cited in the code where it is implemented.

### Profiles

- `Profile::LinuxStrict` (default): Linux syscall semantics, with POSIX's guarantees and nothing beyond them for
  directory durability. A directory operation is durable only after an `fsync` of its parent directory, like
  btrfs and like POSIX. It does not ride along with an unrelated `fsync`.
- `Profile::LinuxExt4`: as strict, but any `fsync` commits the whole jbd2 transaction, so every earlier
  metadata operation becomes durable with it (ext4 `fsync` → `jbd2_complete_transaction`).
- `Profile::MacOs`: `fsync` and `fdatasync` push to the device but do not flush its volatile cache, so they do
  not survive power loss. `fcntl(F_FULLFSYNC)` does. See fsync(2) on macOS: "fsync() ... will not ... flush the
  drive's cache ... use F_FULLFSYNC".

### Durability layers

Each inode has three images: the **page cache** (what reads see), the **device cache** and the **media**.
`write` changes the page cache. Under Linux profiles, `fsync`, `fdatasync`, `O_DSYNC`, `O_SYNC` and
`F_FULLFSYNC` copy page cache to media. Under macOS, `fsync`, `fdatasync`, `O_DSYNC` and `O_SYNC` copy to the
device cache, and only `F_FULLFSYNC` reaches media. `fdatasync` persists the size when needed to read the data
(fdatasync(2)). The model does not track timestamps, so here `fdatasync` and `fsync` act the same on data.
Power loss resolves from the media, OS crash from the device cache, and kill -9 changes nothing durable.

### Data on crash

Rules, each from the kernel's writeback behaviour:

- **Any subset of dirty pages may already have been written back** at any moment before the crash. Writeback is
  asynchronous: `Documentation/admin-guide/sysctl/vm.rst` (dirty_expire_centisecs) and `mm/page-writeback.c`.
  At a crash, each page whose page cache differs from the baseline independently becomes **new** or **old**.
  The probability is drawn per page, and choice 0 is old, the calm outcome for a crash.
- **Torn at the atomic write unit.** With the `TornWrites` class enabled, a page whose fate is new can instead
  be **torn**: each `Config::atomic_write_unit` sector (default 512 bytes; 4 KiB models NVMe with AWUPF covering
  a page) is new or old independently. A page is 4 KiB, and writeback submits pages as bios. Without
  power-loss protection the device guarantees atomicity only per unit: NVMe 1.4 §6.4 "Atomic Operations"
  (AWUN/AWUPF), and `Documentation/block/queue-sysfs.rst` (`atomic_write_unit_max_bytes`).
- **Size.** The durable size after a crash is one of the sizes the inode had since its last durable size, drawn
  from the history. Under ordered-mode semantics (the default), a size extension is committed only after the
  data it covers: ext4 `data=ordered` (`Documentation/filesystems/ext4.rst`, "data=ordered"). So pages between
  the old durable size and the chosen size are forced new. With the `ZerosPastEof` class (XFS, ext4
  `data=writeback`), the size can survive while those pages read as zeros.
- **mmap `MAP_SHARED` stores** are page-cache writes. Their dirtiness is detected by comparing pages, so they
  persist page by page like any dirty page, and `msync(MS_SYNC)` or `fsync` makes them durable (msync(2)).

### Directory operations

`create`, `rename`, `unlink`, `mkdir` and `rmdir` apply to the page-cache namespace at once and are recorded as
pending directory operations in program order. `rename` is atomic: no crash state shows both names or neither,
per rename(2) ("If newpath already exists, it will be atomically replaced"). It is durable only once its parent
directory (both parents, for a cross-directory rename) has been `fsync`ed, per fsync(2): "Calling fsync() does
not necessarily ensure that the entry in the directory containing the file has also reached disk. For that an
explicit fsync() on a file descriptor for the directory is also needed." On a crash, the surviving pending
operations are every synced one plus a drawn **prefix** of the rest, in program order. Journaling file systems
commit metadata in transaction order (jbd2, the XFS log), so a later unsynced operation never survives an
earlier one. Under `LinuxExt4`, any `fsync` makes every earlier operation durable. An unlinked inode that is
still open lives until its last close and is gone after a crash (the orphan list).

### Errors

- **fsync EIO** (`FsyncEio` class): `fsync` returns `EIO` once per file description that sampled the inode's
  error sequence before the failure. A description opened later still sees an error that nobody has seen yet.
  This is `lib/errseq.c`, `errseq_check_and_advance`, and `Documentation/filesystems/vfs.rst` ("Handling errors
  during writeback"). The dirty pages are each drawn as written or dropped, and in either case are marked clean.
  So a later `fsync` succeeds while the dropped pages exist only in the page cache, and a crash loses them
  ("fsyncgate", 2018; `mm/filemap.c` `filemap_check_errors`).
- **ENOSPC** (`Enospc` class): a `write` that allocates new blocks can fail with `ENOSPC`, or write a short
  prefix and return its length (write(2): "the number of bytes written may be less than count if ... there is
  insufficient space"). The next write then fails with `ENOSPC`. `Config::capacity` is a hard limit that applies
  whether or not the class is on. With `EnospcAtFsync`, delayed allocation can report `ENOSPC` at `fsync`
  (ext4 delalloc, `Documentation/filesystems/ext4.rst` "delalloc"). It follows the EIO path, so the pages are
  dropped.
- **Short reads and writes** happen only where Linux allows them for regular files: a read is short only at
  EOF, and a write only on ENOSPC or `EFBIG`. A local file system does not split a regular-file `read` or
  `write` because of a signal; signal(7) applies to "slow" devices.

### Locks

- **flock**: locks belong to the open file description and conflict between descriptions. They are released
  when the last fd of the description closes or when the process dies (flock(2)). `LOCK_NB` gives
  `EWOULDBLOCK`; a blocking request waits, and a handled signal interrupts it with `EINTR`.
- **fcntl (POSIX record) locks**: owned by `(pid, inode)` with byte ranges. They are released when the process
  closes **any** fd for that inode, per fcntl(2) "the locks are lost when ... closes any file descriptor
  referring to the file", which is a famous trap. `F_SETLK` gives `EAGAIN` and `F_SETLKW` waits.

### mmap

`mmap(fd, offset, len, MAP_SHARED, PROT_READ|PROT_WRITE)` returns a pointer straight into the inode's page cache.
The inode's buffer is pinned (allocated once, zeroed, with a reserved capacity, `Config::mmap_reserve`, default
64 MiB, so later growth does not move it). This makes stores through the mapping coherent with `read` and
`write`, as Linux's unified page cache is. Access beyond the mapped length, or past EOF, is the program's bug:
Linux raises `SIGBUS` there (mmap(2)). The facade checks bounds and panics, naming the mapping. `munmap` and
process death drop the mapping, and the stores stay in the page cache. `msync(MS_SYNC)` persists the range.
`MAP_PRIVATE` is copy-on-write and never persists.

### Clock

`clock::Clock` holds the monotonic and realtime clocks and the timers. Monotonic time advances by
`Config::tick` (default 1 µs) per step and by a per-syscall cost (default 10 µs; `fsync` 1 ms). It jumps to the
earliest timer when nothing is runnable. It never goes backward, per clock_gettime(2) on `CLOCK_MONOTONIC`. A
sleep or a timed wait returns no earlier than its deadline, and returns late by a drawn amount (up to 10 ms)
with the `LateWakeups` class (nanosleep(2): "the sleep may be lengthened slightly"). `SystemTime` is monotonic
plus an offset that the `ClockJumps` class shifts by a drawn ±1 h at a drawn step, as an NTP step or an admin
can do (clock_settime(2) on `CLOCK_REALTIME`).

### Swarm

At `Sim::new`, each fault class in `Config::faults` draws one coin: false (the simplest choice) means
**enabled**. An all-zero tape therefore runs with every class on, while every individual fault draw still
defaults to no fault. Shrinking turns off the classes a failure does not need, and the trace lists the classes
that were on. Classes: `Kill9`, `PowerLoss`, `OsCrash`, `TornWrites`, `ZerosPastEof`, `FsyncEio`, `Enospc`,
`EnospcAtFsync`, `LateWakeups`, `ClockJumps`, `SpuriousWakeups`, `AsyncSignals`, `ReaddirShuffle`,
`RandomPreemption`.

## API

```rust
use hegel::sim::{Config, Fault, Outcome, Sim};

#[hegel::test(test_cases = 500)]
fn rows_are_exactly_once(tc: hegel::TestCase) {
    let ghost = Ghost::default();
    let mut sim = Sim::new(&tc, Config::default().faults([Fault::Kill9, Fault::PowerLoss]));
    sim.spawn("app", app(ghost.clone()));
    while let Outcome::Crashed(_) = sim.run() {
        let version = sim.pick("version", &[app_v1, app_v2]);
        sim.spawn("app", version(ghost.clone()));
    }
    sim.spawn("check", check(ghost));
    sim.run().expect_exited();
}
```

- `Sim::new(&dyn Choices, Config)` draws the swarm and the scheduler parameters.
- `spawn(name, program) -> Pid`; `kill(pid)`; `power_loss()`; `os_crash()`; `signal(pid, sig)`.
- `run() -> Outcome`: `Exited(Vec<Exit>) | Crashed(CrashKind)`. A deadlock, the step bound, a panic in any
  simulated thread and a failed `always` all panic with the trace, which fails the Hegel case. (So a panicking
  simulated thread fails the run instead of surfacing as `Err` from `join`, unlike std: under test a panic is
  a finding.)
- `calm(|sim| ..)`: run setup with every fault class and crash off.
- `pick(label, &[T]) -> &T`, `draw_bool(label, p)`, `draw_int(label, lo, hi)`: draws through the same port, noted
  in the trace.
- Oracle: `hegel_sim::oracle::always(cond, "name")` (fails the case with the trace),
  `sometimes(cond, "name")` (reachability, listed in `Sim::report()`), `unreachable("name")`.
- `Sim::read_file(path)`, `Sim::list(dir)`: the page-cache namespace and contents (after a crash, the resolved
  state), read-only, for assertions made from outside.
- `hegel_sim::fork::explore(&mut sim, n, seed, body)`: the native snapshot (see "Snapshots").

Facade deviations from std, each deliberate: `sync_channel(0)` is a one-slot buffer, not a rendezvous;
`File::try_clone` is `Unsupported` under the simulation (no `dup` in the model); `thread::scope` is not
provided.

### Trace and replay

The trace records what explains a failure: swarm classes, spawns, exits, every preemption (the step, from,
to, which change point), every fault that fired (the syscall, the path, the errno or short count), every
crash (its kind, and its resolution per file: pages lost, torn, the size chosen, the directory-op prefix
kept), and every signal delivery. Steps that were not preempted are not listed. When a case fails, the trace
goes out through Hegel's `note`, so it prints only for the final, minimal replay, next to Hegel's
reproduction blob. `HEGEL_SEED` or `#[hegel::reproduce_failure]` replays it exactly. The run is deterministic:
no hash-map order, addresses or wall clock reach any decision. Atomics are named by first-touch ordinal, not by
address.

## Weak memory

Both drivers run the program sequentially consistent. Natively, every operation runs on one OS thread in
program order, so no reordering is observable. In wasm, every atomic is SeqCst by the threads proposal, and LLVM
erases Rust's orderings before codegen. A missing `Acquire` or `Release` is therefore **out of scope** for this
harness. That is covered by ix's `weakmem` model: loom's C11 model with vector clocks, where every load choice
is a Hegel draw. Code under test imports from a facade, and in a weakmem build that facade forwards each
operation with its `Ordering`. The plan is for `hegel_sim::sync::atomic` to forward to a weakmem session under a
`weakmem` feature, which composes the two: this scheduler picks threads, and weakmem picks which store each
load reads. That is not part of these phases.

## Phase 2: the whole world in wasm, snapshot and fork

Design change (2026-11-04, from the owner): the simulated world must be snapshot-able and fork-able, so a test can
run to an interesting state once and then explore many crash points or schedules from it without rerunning the
prefix. Everything below follows from one rule: **all world state lives in wasm linear memory**, and the host
holds almost nothing.

### Pieces

- **The machine module.** `hegel-sim-core` (kernel, scheduler, clock, swarm, trace) compiled to
  `wasm32-unknown-unknown` behind a small C ABI (`sim/machine`). One instance is one simulated machine, and its
  linear memory holds the whole machine. Its only imports are the draws (`hegel.int(lo, hi)`, `hegel.coin(p)`),
  answered by the host from the case's `Choices`. Its exports take and return requests in a fixed binary encoding
  (`kernel::wire`) through a buffer in its own memory: `syscall(pid, tid)`, `crash(kind)`, `pick()`, `block`,
  `wake`, `spawn`, `exit`, `trace`. Because phase 1 already builds the core for both targets, the native driver
  and the machine module run the same code.
- **Process modules.** A program is a `wasm32-wasip1-threads` module. A process is one instance of it with its
  own memory; its WASI imports (and the extra `hegel.mmap`, `hegel.msync`, `hegel.flock`, `hegel.signal`
  imports) go to the host, which only encodes the call, hands it to the machine instance, and writes the result
  back. Threads of a process are coroutines inside that one instance (std's `thread::spawn` calls
  `wasi.thread-spawn`; the host starts a coroutine at the instance's `wasi_thread_start(tid, arg)`, and wasi-libc
  has already placed that thread's stack and TLS in linear memory). kill -9 drops the instance. Power loss calls
  the machine's `crash`. Restart, with the same or another version, instantiates a module against the same
  machine.
- **Load-time instrumentation** (walrus): `call $hegel.yield` before every atomic instruction
  (`*.atomic.*` load, store, RMW, cmpxchg, `atomic.fence`); `memory.atomic.wait*`/`notify` replaced by
  `hegel.wait*`/`hegel.notify` with the memarg offset folded into the address (a real wait would block the only
  OS thread); every mutable global exported (`__stack_pointer`, `__tls_base`, ...) so a snapshot can read and
  restore it.
- **The host** (`sim/wasm-host`, upstream Wasmtime, synchronous API) is the scheduler's choice point (it asks the
  machine whom to run and switches to that coroutine), the draw port, and the snapshot manager. Its own state is
  a few tables: instances by pid, coroutine entry points by tid, pending replies. It is serializable by design.

### Stack switching: one mode per run, chosen explicitly

A coroutine's suspended stack must also live in memory the snapshot covers. Two modes, selected by
`WasmConfig::stacks(StackMode::..)`; there is no fallback between them, and asking for a mode the host cannot do
stops with an error that names the mode and the platform.

- **2a: `StackMode::Asyncify`.** Binaryen's Asyncify pass (through the `wasm-opt` crate, which bundles binaryen)
  rewrites the program so that an import listed in `asyncify-imports` can unwind the whole wasm stack into a
  buffer in linear memory and later rewind it. The listed imports are `hegel.yield`, `hegel.wait*` and every WASI
  and `hegel.*` syscall, since each is a switch point. The unwind buffers live in pages the host grows into the
  process's memory. A switch is an unwind of the running coroutine and a rewind of the next one. Works on
  upstream Wasmtime on every platform it supports (including aarch64 macOS). Cost: code size and a check after
  every call that can reach a switch point, plus a rewind proportional to stack depth per switch. The overhead
  is measured on a blaze-like workload (hash maps, sorting, string building, many small allocations) and
  recorded in "Measurements" below.
- **2b: `StackMode::Continuations`.** The stack-switching proposal (typed continuations), executed by our
  Wasmtime fork, `github.com/indexable-inc/wasmtime` (public), branch `sim-stacks`. A switch is a native stack
  switch, close to free per switch, with no code-size cost. Continuation stacks come from a region the snapshot
  manager owns (`Config::with_continuation_stacks`, landed on `sim-stacks` as a26d24c79), so a snapshot is the
  linear memories plus that region. The program is compiled with the proposal's instructions by a load-time
  rewrite of `hegel.yield` into `suspend`, and of each coroutine entry into `cont.new` plus `resume`.
  Why both modes stay: Asyncify runs on every platform and on upstream Wasmtime today; continuations are the
  fast path but Wasmtime implements them only on x86_64 Unix so far. A run states which mode it wants.

  What 2b still needs from the fork:
  1. *done:* an embedder hook for continuation stack memory, and the list of stack ranges per store.
  2. `VMContRef` records (today a `Box` per continuation on the host heap) allocated in the same region, so the
     region alone describes every suspended continuation.
  3. Restorable stacks. A suspended native stack holds return addresses into compiled code and pointers to the
     instance's vmctx. Either pin the region, the code and the vmctx at fixed addresses (one address-space
     layout for every process that restores the snapshot), or, on restore, walk each stack with Cranelift's
     stack maps and frame-pointer chain and rebase those pointers. Pinning is simpler and is the first plan.
  4. An aarch64 backend for stack switching (the Mac is aarch64). Until then, 2b runs in an x86_64 ix VM.
  The fork is public, so hegel depends on it by git revision under a non-default feature (`continuations`) of
  `hegel-sim-wasm`; the default build stays on upstream Wasmtime with Asyncify. Moving 2b upstream is the long
  term home for items 1 to 3.

### Snapshots

```rust
let mut world = World::new(&tc, WasmConfig::default())?;
world.spawn("app", &app_v1)?;
world.run_until(|w| w.step() >= 500)?;
let snap = world.snapshot();                 // CoW pages + host tables, content-hashed
let id = snap.id();                          // blake3 over the manifest: equal worlds, equal ids
let outcomes = world.fork(&snap, 64, |w, i| { w.reseed(i); w.run() })?;
let d = Snapshot::diff(&before, &after);     // pages changed per memory, host-table changes, files changed
snap.save(&store_dir)?;                       // pages stored once by hash; ship the dir anywhere
let again = Snapshot::load(&store_dir, id)?;
```

- A **snapshot** is every linear memory (machine and processes) split into 4 KiB pages, each stored once in a
  page store keyed by its blake3 hash, plus the host tables, plus every exported global. Taking one costs a hash
  per page changed since the last snapshot (a dirty-page scan against the previous snapshot), so many snapshots
  of one run share almost every page.
- **Fork** restores a snapshot into a fresh `Store` (instantiate each module, overwrite its memory and globals
  from the pages) as many times as asked, in one process. Branches share the page store, so 64 branches of a
  100 MB world cost 64 copies of only the pages each branch writes once they run (pages are copied in on
  restore; mapping them copy-on-write is the follow-up once a custom `MemoryCreator` is wired).
- **Choices.** A snapshot records the choice prefix that led to it. A branch draws its suffix from its own source
  (a seed, or Hegel's `explore::Source`), and a failing branch replays from scratch as prefix plus suffix, so
  Hegel can still shrink it (over the suffix first).
- **Diff** compares page hashes per memory, then decodes what changed into the machine's own terms through its
  `dump` export: files and directories added, removed or changed, per durability layer.
- **Save and ship.** The page store is a directory of hash-named pages plus manifests. A snapshot is valid in
  any process with the same module bytes (whose hashes are in the manifest).

### Native snapshots (day one)

The native driver cannot copy its world (fiber stacks hold raw pointers into the host's heap), so phase 1
provides `hegel_sim::fork::explore`: fork the whole host process at the snapshot point, AFL fork-server style,
run one branch per child with seeded choices, collect each outcome over a pipe, and re-run a failing branch
in-process with `fork::replay`. Limits: no save, no dedup, no shipping, one process per branch, and the usual
`fork()` constraints (only the calling thread survives, so run with `--test-threads=1` when other host threads
may hold locks; macOS frameworks that refuse to run after fork must not be touched in a child).

### Measurements

Recorded here as phase 2a lands: Asyncify's code-size and run-time overhead on the blaze-like workload, the cost
per switch in each mode, and snapshot and restore cost per MB.

## Phase 3: validating the model against real kernels

`sim/validation/validate_kernel_model.py` (stdlib Python, one JSON config parsed into a frozen dataclass) runs on
a Linux VM:

1. Create a loop device and a `dm-log-writes` target over it (`Documentation/admin-guide/device-mapper/log-writes.rst`),
   then `mkfs` it with the chosen file system (ext4 ordered, ext4 writeback, xfs, btrfs) and mount it.
2. Run each workload: a JSON list of syscalls in the same vocabulary as `kernel::Syscall`, executed by a tiny
   stdlib Python runner with `os.write`, `os.fsync`, `os.rename` and so on. Insert a log mark after each
   syscall.
3. Unmount, then use `replay-log` from xfstests (`src/log-writes`) to replay the log to every mark and every
   flush/FUA. Between two flushes it also replays CrashMonkey-style subsets of the writes in flight, all for
   small windows and sampled for large ones. Mount each crash state read-only, or after journal recovery, and
   dump the namespace and file contents.
4. Feed each observed state to the model's membership checker (`cargo run -p hegel-sim-core --example
   crash_states`). The checker answers whether the model can produce that state from the workload (per-page
   fate, size history, directory prefix). A state the model cannot produce is a model bug that misses real
   behaviour. A model state never observed across many runs is reported as "allowed, not seen". That is
   conservative but legal, unless the kernel documents it as impossible.

The output is a JSON report per file system. The rules are evidence-backed once each one has an observed
state that needs it, and no observed state falls outside the model.

## Phases

| Phase | Scope | Status |
|---|---|---|
| 1 | `hegel-sim-core` (kernel, PCT, clock, swarm, trace; builds for `wasm32-unknown-unknown`), `hegel-sim` facade and native driver with crash and restart, the oracle, native `fork()` snapshots, `hegeltest` `sim` feature, `hegel-sim-examples` with the six planted bugs (buggy caught, fixed passing) | landed |
| 2a | `hegel-sim-machine` (the kernel as a wasm module), `hegel-sim-wasm` (Wasmtime host, atomic-yield instrumentation, Asyncify stack switching, WASI p1 over the machine), the snapshot manager (snapshot, fork N, diff, save), a planted-bug wasm example, Asyncify overhead measured | building |
| 2b | Stack switching from our Wasmtime fork (`indexable-inc/wasmtime`, branch `sim-stacks`) with continuation stacks in a snapshot-owned region | planned |
| 3 | `sim/validation`: dm-log-writes recorder and crash-state replayer, model membership checker | written, not run |
