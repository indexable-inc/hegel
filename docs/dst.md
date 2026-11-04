---
status: plan
phases:
  phase-1-native: building
  phase-2-wasi: planned
  phase-3-validation: planned
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
| `hegel-wasi` | `sim/wasi` | Phase 2: a WASI preview 1 host on Wasmtime (async, fibers) whose every import goes to the `hegel-sim-core` kernel, plus the atomic-yield instrumentation pass. |
| `hegel-sim-examples` | `sim/examples` | `publish = false`. The six planted bugs as tiny programs, each with a buggy and a fixed variant, and the Hegel tests that catch the buggy variant and pass the fixed one. |
| validation | `sim/validation` | Phase 3: `validate_kernel_model.py` and the model's crash-state membership checker. |

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
- `run() -> Outcome`: `Exited(Vec<(Pid, i32)>) | Crashed(CrashReport)`. A deadlock, the step bound, a thread
  panic and a failed `always` all panic with the trace, which fails the Hegel case.
- `pick(label, &[T]) -> &T`, `draw_bool(label, p)`, `draw_int(label, lo, hi)`: draws through the same port, noted
  in the trace.
- Oracle: `hegel_sim::oracle::always(cond, "name")` (fails the case with the trace),
  `sometimes(cond, "name")` (reachability, listed in `Sim::report()`), `unreachable("name")`.
- `Sim::fs_view()`: the page-cache namespace and contents, read-only, for assertions made from outside.

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

## Phase 2: wasm

`hegel-wasi` runs a `wasm32-wasip1-threads` module on Wasmtime with `async_support`:

- **Instrumentation at load time** (walrus): before every atomic instruction (`i32/i64.atomic.*` load, store,
  RMW and cmpxchg, plus `atomic.fence`), insert `call $hegel.yield`. Replace `memory.atomic.wait32/64` and
  `memory.atomic.notify` with calls to the host's `hegel.wait32/64` and `hegel.notify`, adding the memarg
  offset to the address, so the futex lives in the scheduler. A real wait would block the only OS thread.
- **Threads are fibers.** Each wasm thread is its own `Store` sharing the module's `SharedMemory`, as in
  wasi-threads. Its `wasi_thread_start` runs as a future on the host's deterministic executor. The imported
  `wasi.thread-spawn` instantiates the new store. `hegel.yield` is an async host function. It returns `Pending`
  exactly when the shared PCT core says to switch, and Wasmtime's fiber suspends the guest. The same
  `hegel-sim-core` scheduler, kernel and clock drive it, so a wasm case and a native case shrink and replay
  the same way.
- **WASI preview 1** is our own implementation over the kernel model: files, directories, `fd_sync` and
  `fd_datasync`, `path_rename`, clocks from the virtual clock, `poll_oneoff` sleeps, `random_get` from a
  Hegel draw, and `proc_exit`. Calls with no model return `ENOSYS`, which is a real errno, and the call's name
  is traced. `hegel.flock` adds locks, and the guest declares it as an extern import.
- **Processes and versions**: each process instantiates a module (its own memory). Restart can instantiate a
  different module, which is the version-skew test for wasm. kill -9 drops the process's futures, and Wasmtime
  unwinds the suspended guest stacks.
- Out of scope in wasm: mmap (WASI has none), signals (none), and weak memory (see above).

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
| 1 | `hegel-sim-core` (kernel, PCT, clock, swarm, trace), `hegel-sim` facade and native driver with crash and restart, the oracle, `hegeltest` `sim` feature, `hegel-sim-examples` with the six planted bugs (buggy caught, fixed passing) | building |
| 2 | `hegel-wasi`: Wasmtime async host, atomic-yield instrumentation, WASI p1 over the kernel, threads as fibers, versioned restart, a planted-bug wasm example | planned |
| 3 | `sim/validation`: dm-log-writes recorder and crash-state replayer, model membership checker | planned (written, not run) |
