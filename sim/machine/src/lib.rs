//! One simulated machine as a wasm module: Hegel's Linux kernel model, PCT
//! scheduler, virtual clock, fault swarm and trace behind scalar exports.
//!
//! The host owns the simulated processes (each its own wasm instance) and calls
//! in at every switch point. Bulk data crosses through one scratch buffer in
//! this module's memory ([`buf_ptr`], [`buf_cap`]): the host writes a request
//! there, calls an export, and reads the answer back. Syscalls and their results
//! use [`hegel_sim_core::kernel::wire`]. Every choice is a call to the host's
//! `hegel.int` and `hegel.coin` imports, so the host's test case decides and
//! shrinks the run. Natively (for tests) choices come from a seeded
//! [`SeededChoices`]; see [`seed`].
//!
//! The logic mirrors `hegel-sim`'s native driver (`rt.rs`) without fibers and
//! signals.

use std::cell::UnsafeCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;

use hegel_sim_core::clock::TimerId;
use hegel_sim_core::kernel::{CrashKind, Env, Kernel, KernelConfig, Profile, wire};
use hegel_sim_core::sched::{Pick, Preemption, SchedConfig};
use hegel_sim_core::trace::fmt_ns;
use hegel_sim_core::{
    Choices, Clock, Fault, FaultSet, Pid, Rates, Sched, Swarm, SysResult, Tid, Trace, WaitKey,
};

/// Size of the scratch buffer, 1 MiB.
pub const BUF_CAP: usize = 1 << 20;

/// Bytes of the `init` configuration record.
pub const CONFIG_LEN: usize = 1 + 8 + 8 + 4 + 1 + 4 + 4 + 8;

#[cfg(target_arch = "wasm32")]
mod host {
    #[link(wasm_import_module = "hegel")]
    unsafe extern "C" {
        #[link_name = "int"]
        fn hegel_int(lo: i64, hi: i64) -> i64;
        #[link_name = "coin"]
        fn hegel_coin(p: f64) -> i32;
    }

    /// [`Choices`](hegel_sim_core::Choices) answered by the host's imports.
    #[derive(Debug)]
    pub struct HostChoices;

    impl hegel_sim_core::Choices for HostChoices {
        fn int(&self, _label: &'static str, lo: u64, hi: u64) -> u64 {
            // SAFETY: a plain host import taking and returning scalars.
            unsafe { hegel_int(lo as i64, hi as i64) as u64 }
        }
        fn coin(&self, _label: &'static str, p: f64) -> bool {
            // SAFETY: a plain host import taking and returning scalars.
            unsafe { hegel_coin(p) != 0 }
        }
        fn note(&self, _line: &str) {}
    }
}

#[cfg(target_arch = "wasm32")]
type Port = host::HostChoices;
#[cfg(not(target_arch = "wasm32"))]
type Port = hegel_sim_core::SeededChoices;

struct Machine {
    choices: Port,
    kernel: Kernel,
    sched: Sched,
    clock: Clock,
    swarm: Swarm,
    trace: Trace,
    tick_ns: u64,
    crashes_left: u32,
    crash_rate: Option<f64>,
    procs: BTreeMap<Pid, ()>,
    timers: BTreeMap<Tid, TimerId>,
}

struct State {
    machine: Option<Machine>,
    buf: Vec<u8>,
    last_len: u32,
    #[cfg(not(target_arch = "wasm32"))]
    seed: u64,
}

impl State {
    const fn new() -> State {
        State {
            machine: None,
            buf: Vec::new(),
            last_len: 0,
            #[cfg(not(target_arch = "wasm32"))]
            seed: 0,
        }
    }

    fn buf(&mut self) -> &mut [u8] {
        if self.buf.is_empty() {
            self.buf = vec![0; BUF_CAP];
        }
        &mut self.buf
    }

    fn machine(&mut self) -> (&mut Machine, &mut [u8]) {
        self.buf();
        let m = self
            .machine
            .as_mut()
            .unwrap_or_else(|| panic!("hegel-sim-machine: an export ran before `init`"));
        (m, &mut self.buf)
    }

    /// Put `bytes` at the start of the buffer, cut to its capacity; returns the length.
    fn put(&mut self, bytes: &[u8]) -> u32 {
        let n = bytes.len().min(BUF_CAP);
        self.buf()[..n].copy_from_slice(&bytes[..n]);
        self.last_len = n as u32;
        n as u32
    }
}

#[cfg(target_arch = "wasm32")]
struct Global(UnsafeCell<State>);

// SAFETY: wasm32-unknown-unknown without the atomics feature is single-threaded:
// no other thread can observe the cell, and each export takes the one `&mut`
// for its own duration (the host imports never call back into an export).
#[cfg(target_arch = "wasm32")]
unsafe impl Sync for Global {}

#[cfg(target_arch = "wasm32")]
static STATE: Global = Global(UnsafeCell::new(State::new()));

#[cfg(target_arch = "wasm32")]
fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    // SAFETY: see `Global`; `with` is never re-entered, so this is the only borrow.
    f(unsafe { &mut *STATE.0.get() })
}

#[cfg(not(target_arch = "wasm32"))]
thread_local! {
    static STATE: UnsafeCell<State> = const { UnsafeCell::new(State::new()) };
}

#[cfg(not(target_arch = "wasm32"))]
fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    // SAFETY: the cell is this thread's own and `with` is never re-entered (no
    // export calls another), so this is the only borrow.
    STATE.with(|s| f(unsafe { &mut *s.get() }))
}

/// Run `f` on the scratch buffer (native tests read and write it here).
pub fn with_buf<R>(f: impl FnOnce(&mut [u8]) -> R) -> R {
    with(|s| f(s.buf()))
}

fn label(sched: &Sched, tid: Tid) -> String {
    format!("p{}.t{tid}", sched.pid(tid))
}

impl Machine {
    fn log(&mut self, text: String) {
        self.trace
            .push(self.sched.step(), self.clock.mono_ns(), text);
    }

    fn wake_woken(&mut self) {
        for k in self.kernel.take_woken() {
            self.sched.wake(k);
        }
    }

    fn exit_thread(&mut self, tid: Tid) {
        self.sched.exit(tid);
        self.sched.wake(WaitKey::Join(tid));
        if let Some(id) = self.timers.remove(&tid) {
            self.clock.cancel(id);
        }
    }

    fn draw_crash_rate(&mut self) {
        let crash_classes = Fault::CRASHES.iter().any(|f| self.swarm.on(*f));
        self.crash_rate = Some(if self.crashes_left > 0 && crash_classes {
            let e = self.choices.int("crash.rate_exp", 1, 11);
            1.0 / (1u64 << e) as f64
        } else {
            0.0
        });
    }

    /// nanosleep(2): "the sleep may be lengthened slightly"; never shortened.
    fn lateness(&self, deadline: u64) -> u64 {
        if self.swarm.fire(
            &self.choices,
            Fault::LateWakeups,
            self.swarm.rates.late_wakeup,
        ) {
            deadline + self.choices.int("clock.late_ns", 1, 10_000_000)
        } else {
            deadline
        }
    }
}

fn read_config(b: &[u8]) -> Result<(KernelConfig, u32, bool, FaultSet, u32, u64), String> {
    if b.len() != CONFIG_LEN {
        return Err(format!(
            "init: the configuration record is {} bytes, expected {CONFIG_LEN}",
            b.len()
        ));
    }
    let u32_at = |o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let u64_at = |o: usize| u64::from(u32_at(o)) | (u64::from(u32_at(o + 4)) << 32);
    let profile = match b[0] {
        0 => Profile::LinuxStrict,
        1 => Profile::LinuxExt4,
        2 => Profile::MacOs,
        p => {
            return Err(format!(
                "init: profile byte {p} at offset 0 names no profile (0 LinuxStrict, 1 LinuxExt4, 2 MacOs)"
            ));
        }
    };
    let swarm = match b[21] {
        0 => false,
        1 => true,
        s => {
            return Err(format!(
                "init: swarm byte {s} at offset 21 is neither 0 nor 1"
            ));
        }
    };
    let bits = u32_at(22);
    let unknown = bits & !((1u32 << Fault::ALL.len()) - 1);
    if unknown != 0 {
        return Err(format!(
            "init: fault bits {unknown:#x} at offset 22 name no fault class"
        ));
    }
    let faults: FaultSet = Fault::ALL
        .iter()
        .enumerate()
        .filter(|(i, _)| bits & (1 << i) != 0)
        .map(|(_, f)| *f)
        .collect();
    let kernel = KernelConfig {
        profile,
        capacity: u64_at(1),
        atomic_write_unit: u64_at(9),
        ..KernelConfig::default()
    };
    Ok((kernel, u32_at(17), swarm, faults, u32_at(26), u64_at(30)))
}

/// The scratch buffer's address in this module's memory.
#[unsafe(no_mangle)]
pub extern "C" fn buf_ptr() -> u32 {
    with(|s| s.buf().as_mut_ptr() as usize as u32)
}

/// The scratch buffer's size, 1 MiB.
#[unsafe(no_mangle)]
pub extern "C" fn buf_cap() -> u32 {
    BUF_CAP as u32
}

/// Length of the last message written to the buffer (an error, or the deadlock report).
#[unsafe(no_mangle)]
pub extern "C" fn last_len() -> u32 {
    with(|s| s.last_len)
}

/// Seed the native choice source for the next [`init`] (native builds only).
#[cfg(not(target_arch = "wasm32"))]
#[unsafe(no_mangle)]
pub extern "C" fn seed(seed: u64) {
    with(|s| s.seed = seed);
}

/// Boot the machine from the `CONFIG_LEN`-byte record in `buf[0..cfg_len]`:
/// profile u8 (0 LinuxStrict, 1 LinuxExt4, 2 MacOs), capacity u64,
/// atomic_write_unit u64, max_depth u32, swarm u8 (1 draws the swarm, 0 enables
/// every allowed class), faults u32 (bit `i` allows `Fault::ALL[i]`),
/// max_crashes u32, tick_ns u64, all little-endian. Draws the swarm, then the
/// scheduler. Returns 0, or -1 with a message in the buffer (`last_len` bytes).
#[unsafe(no_mangle)]
pub extern "C" fn init(cfg_len: u32) -> i32 {
    with(|s| {
        let n = (cfg_len as usize).min(BUF_CAP);
        let (kcfg, max_depth, draw, faults, max_crashes, tick_ns) = match read_config(&s.buf()[..n])
        {
            Ok(c) => c,
            Err(msg) => {
                s.put(msg.as_bytes());
                return -1;
            }
        };
        #[cfg(target_arch = "wasm32")]
        let choices = host::HostChoices;
        #[cfg(not(target_arch = "wasm32"))]
        let choices = hegel_sim_core::SeededChoices::new(s.seed);
        let swarm = if draw {
            Swarm::draw(&choices, faults, Rates::default())
        } else {
            Swarm::fixed(faults, Rates::default())
        };
        let sched = Sched::new(
            &choices,
            &SchedConfig {
                max_depth,
                steps_hint: None,
                random_walk: swarm
                    .on(Fault::RandomPreemption)
                    .then_some(swarm.rates.random_preemption),
            },
        );
        let mut trace = Trace::new();
        let classes: Vec<String> = swarm.enabled.iter().map(|f| format!("{f:?}")).collect();
        trace.push(
            0,
            0,
            format!(
                "swarm: {}",
                if classes.is_empty() {
                    "no fault classes".into()
                } else {
                    classes.join(", ")
                }
            ),
        );
        if !sched.change_points().is_empty() {
            trace.push(
                0,
                0,
                format!(
                    "pct: change points {:?} over horizon {}",
                    sched.change_points(),
                    sched.horizon()
                ),
            );
        }
        s.machine = Some(Machine {
            choices,
            kernel: Kernel::new(kcfg),
            sched,
            clock: Clock::new(),
            swarm,
            trace,
            tick_ns,
            crashes_left: max_crashes,
            crash_rate: None,
            procs: BTreeMap::new(),
            timers: BTreeMap::new(),
        });
        0
    })
}

/// Redraw the swarm, the scheduler and the crash budget from a new config
/// record (same layout as [`init`]), keeping the kernel, clock and processes:
/// a world restored from a snapshot draws its own chaos from here. Needs no
/// live thread, and the kernel fields must match the running kernel's. Returns
/// 0, or -1 with a message in the buffer.
#[unsafe(no_mangle)]
pub extern "C" fn reconfigure(cfg_len: u32) -> i32 {
    with(|s| {
        let n = (cfg_len as usize).min(BUF_CAP);
        let parsed = read_config(&s.buf()[..n]);
        let (m, _) = s.machine();
        let refusal = match &parsed {
            Err(msg) => Some(msg.replacen("init:", "reconfigure:", 1)),
            Ok(_) if m.sched.live().next().is_some() => {
                Some("reconfigure: threads are still live; reconfigure between runs".to_owned())
            }
            Ok((k, ..)) => {
                let have = m.kernel.config();
                (k.profile != have.profile || k.capacity != have.capacity || k.atomic_write_unit != have.atomic_write_unit)
                    .then(|| format!("reconfigure: kernel fields {:?}/{}/{} differ from the running kernel's {:?}/{}/{}", k.profile, k.capacity, k.atomic_write_unit, have.profile, have.capacity, have.atomic_write_unit))
            }
        };
        if let Some(msg) = refusal {
            s.put(msg.as_bytes());
            return -1;
        }
        let Ok((_, max_depth, draw, faults, max_crashes, tick_ns)) = parsed else {
            unreachable!()
        };
        let (m, _) = s.machine();
        m.swarm = if draw {
            Swarm::draw(&m.choices, faults, Rates::default())
        } else {
            Swarm::fixed(faults, Rates::default())
        };
        m.sched = Sched::new(
            &m.choices,
            &SchedConfig {
                max_depth,
                steps_hint: None,
                random_walk: m
                    .swarm
                    .on(Fault::RandomPreemption)
                    .then_some(m.swarm.rates.random_preemption),
            },
        );
        m.tick_ns = tick_ns;
        m.crashes_left = max_crashes;
        m.crash_rate = None;
        let classes: Vec<String> = m.swarm.enabled.iter().map(|f| format!("{f:?}")).collect();
        m.log(format!(
            "reconfigure: swarm {}",
            if classes.is_empty() {
                "no fault classes".into()
            } else {
                classes.join(", ")
            }
        ));
        if !m.sched.change_points().is_empty() {
            let cp = format!(
                "pct: change points {:?} over horizon {}",
                m.sched.change_points(),
                m.sched.horizon()
            );
            m.log(cp);
        }
        0
    })
}

/// Register process `pid` with an empty descriptor table.
#[unsafe(no_mangle)]
pub extern "C" fn proc_spawn(pid: u32) {
    with(|s| {
        let (m, _) = s.machine();
        m.kernel.spawn(pid);
        m.procs.insert(pid, ());
        m.log(format!("spawn process [{pid}]"));
    });
}

/// Process `pid` exited: close its descriptors (releasing its locks), wake what
/// that wakes, and end any of its threads still live.
#[unsafe(no_mangle)]
pub extern "C" fn proc_exit(pid: u32) {
    with(|s| {
        let (m, _) = s.machine();
        for tid in m.sched.live_of(pid) {
            m.exit_thread(tid);
        }
        m.kernel.exit(pid);
        m.wake_woken();
        if m.procs.remove(&pid).is_some() {
            m.log(format!("exit process [{pid}]"));
        }
    });
}

/// Start a runnable thread in `pid` with a drawn priority; returns its tid.
#[unsafe(no_mangle)]
pub extern "C" fn thread_spawn(pid: u32) -> u32 {
    with(|s| {
        let (m, _) = s.machine();
        let tid = m.sched.spawn(&m.choices, pid, "thread");
        m.log(format!("spawn thread p{pid}.t{tid}"));
        tid
    })
}

/// Thread `tid` exited; wakes its joiners.
#[unsafe(no_mangle)]
pub extern "C" fn thread_exit(tid: u32) {
    with(|s| s.machine().0.exit_thread(tid));
}

/// Apply the syscall encoded in `buf[0..len]` for thread `tid` of `pid`. Writes
/// the encoded `SysResult` to the buffer and returns its length; on
/// `Block(key)` the thread is blocked on `key` and retries the call once woken.
/// Returns -1 with a message in the buffer (`last_len` bytes) when the call is
/// malformed or the result does not fit.
#[unsafe(no_mangle)]
pub extern "C" fn syscall(pid: u32, tid: u32, len: u32) -> i32 {
    with(|s| {
        let (m, buf) = s.machine();
        let call = match wire::decode_syscall(&buf[..(len as usize).min(BUF_CAP)]) {
            Ok(c) => c,
            Err(e) => {
                let msg = format!("syscall from {}: {e}", label(&m.sched, tid));
                s.put(msg.as_bytes());
                return -1;
            }
        };
        m.clock.advance(m.tick_ns);
        if m.swarm
            .fire(&m.choices, Fault::ClockJumps, m.swarm.rates.clock_jump)
        {
            let mag = m.choices.int("clock.jump_secs", 1, 3600) as i128 * 1_000_000_000;
            let delta = if m.choices.coin("clock.jump_back", 0.5) {
                -mag
            } else {
                mag
            };
            m.clock.jump_realtime(delta);
            m.log(format!(
                "realtime clock jumps {}{}",
                if delta < 0 { "-" } else { "+" },
                fmt_ns(delta.unsigned_abs() as u64)
            ));
        }
        let who = label(&m.sched, tid);
        let step = m.sched.step();
        let t_ns = m.clock.mono_ns();
        let mut env = Env {
            choices: &m.choices,
            swarm: &m.swarm,
            trace: &mut m.trace,
            step,
            t_ns,
            who: &who,
        };
        let r = m.kernel.syscall(pid, &call, &mut env);
        m.clock.advance(call.cost_ns());
        m.wake_woken();
        if let SysResult::Block(key) = r {
            m.sched.block(tid, key);
        }
        let mut out = Vec::new();
        wire::encode_result(&r, &mut out);
        if out.len() > BUF_CAP {
            let msg = format!(
                "syscall {} from {who}: its {}-byte result exceeds the {BUF_CAP}-byte buffer",
                call.name(),
                out.len()
            );
            s.put(msg.as_bytes());
            return -1;
        }
        s.put(&out) as i32
    })
}

/// Block `tid` on the futex at `addr` in `pid`'s memory.
#[unsafe(no_mangle)]
pub extern "C" fn futex_wait(tid: u32, pid: u32, addr: u32) {
    with(|s| {
        s.machine()
            .0
            .sched
            .block(tid, WaitKey::Futex(pid, u64::from(addr)))
    });
}

/// Wake up to `count` drawn waiters on the futex at `addr` in `pid`'s memory
/// (`u32::MAX` wakes all); returns how many woke.
#[unsafe(no_mangle)]
pub extern "C" fn futex_wake(pid: u32, addr: u32, count: u32) -> u32 {
    with(|s| {
        let (m, _) = s.machine();
        let key = WaitKey::Futex(pid, u64::from(addr));
        if count == u32::MAX {
            return m.sched.wake(key) as u32;
        }
        let mut n = 0;
        while n < count && m.sched.wake_one(&m.choices, key).is_some() {
            n += 1;
        }
        n
    })
}

/// Block `tid` until monotonic `deadline_ns` (possibly later under LateWakeups).
#[unsafe(no_mangle)]
pub extern "C" fn sleep_until(tid: u32, deadline_ns: u64) {
    with(|s| {
        let (m, _) = s.machine();
        let deadline = m.lateness(deadline_ns);
        let id = m.clock.add_timer(deadline, WaitKey::Timer(tid));
        if let Some(old) = m.timers.insert(tid, id) {
            m.clock.cancel(old);
        }
        m.sched.block(tid, WaitKey::Timer(tid));
    });
}

/// `tid` yielded or spun: it runs last until another thread takes a step.
#[unsafe(no_mangle)]
pub extern "C" fn yielded(tid: u32) {
    with(|s| s.machine().0.sched.yielded(tid));
}

/// Choose the next thread to run: returns its tid, -2 when no thread is live,
/// or -3 on deadlock, with every blocked thread described in the buffer
/// (`last_len` bytes). Fires due timers first and, when nothing is runnable,
/// jumps the clock to the next timer.
#[unsafe(no_mangle)]
pub extern "C" fn pick() -> i32 {
    with(|s| {
        let (m, _) = s.machine();
        loop {
            for k in m.clock.fire_due() {
                if let WaitKey::Timer(t) = k {
                    m.timers.remove(&t);
                    m.sched.unblock(t);
                }
            }
            let pick = m.sched.pick(&m.choices);
            if let Some((from, to, why)) = m.sched.last_preemption() {
                let why = match why {
                    Preemption::ChangePoint(i) => format!("change point {i}"),
                    Preemption::Random => "random walk".into(),
                    Preemption::Yield => "yield".into(),
                    Preemption::Priority => "priority".into(),
                };
                let text = format!(
                    "preempt {} at switch point -> {} ({why})",
                    label(&m.sched, from),
                    label(&m.sched, to)
                );
                m.log(text);
            }
            match pick {
                Pick::Run(tid) => {
                    if let Some(id) = m.timers.remove(&tid) {
                        m.clock.cancel(id);
                    }
                    return tid as i32;
                }
                Pick::Done => return -2,
                Pick::Idle if m.clock.jump_to_next() => continue,
                Pick::Idle => {
                    let mut msg = String::from(
                        "deadlock: every live thread is blocked and no timer is pending:\n",
                    );
                    for t in m.sched.live() {
                        let _ = writeln!(
                            msg,
                            "  {} waits on {:?}",
                            label(&m.sched, t),
                            m.sched.status(t)
                        );
                    }
                    s.put(msg.as_bytes());
                    return -3;
                }
            }
        }
    })
}

/// Recompute this epoch's crash rate (`crash.rate_exp`), as the native driver
/// does at the start of each run.
#[unsafe(no_mangle)]
pub extern "C" fn begin_epoch() {
    with(|s| s.machine().0.draw_crash_rate());
}

/// Draw whether to crash at this crash point (`at_syscall` is 1 at a syscall, 0
/// at another switch point; it is recorded in the trace). Draws the epoch's
/// crash rate first if [`begin_epoch`] has not. Returns 0 for no crash, 1 for
/// kill -9 (victim pid in `buf[0..4]`, little-endian), 2 for power loss, 3 for
/// an OS crash. The host then performs it with [`crash`].
#[unsafe(no_mangle)]
pub extern "C" fn maybe_crash(at_syscall: u32) -> i32 {
    with(|s| {
        let (m, buf) = s.machine();
        if m.crash_rate.is_none() {
            m.draw_crash_rate();
        }
        let rate = m.crash_rate.unwrap_or(0.0);
        if m.crashes_left == 0 || rate <= 0.0 {
            return 0;
        }
        if !m.choices.coin("crash.now", rate) {
            return 0;
        }
        let have_procs = !m.procs.is_empty();
        let kinds: Vec<Fault> = Fault::CRASHES
            .into_iter()
            .filter(|f| m.swarm.on(*f) && (*f != Fault::Kill9 || have_procs))
            .collect();
        if kinds.is_empty() {
            return 0;
        }
        let k = kinds[m.choices.int("crash.kind", 0, kinds.len() as u64 - 1) as usize];
        m.crashes_left -= 1;
        let at = if at_syscall != 0 {
            "syscall"
        } else {
            "switch point"
        };
        m.log(format!("crash point at {at}: {k:?}"));
        match k {
            Fault::Kill9 => {
                let live: Vec<Pid> = m.procs.keys().copied().collect();
                let victim = live[m.choices.int("crash.victim", 0, live.len() as u64 - 1) as usize];
                buf[..4].copy_from_slice(&victim.to_le_bytes());
                1
            }
            Fault::OsCrash => 3,
            _ => 2,
        }
    })
}

/// Perform a crash: kind 1 is kill -9 of `pid` (its descriptors close, its
/// threads end; the host drops its instance); 2 is power loss and 3 an OS crash,
/// which end every process and thread and resolve every unsynced page and
/// directory operation. Any other kind is ignored.
#[unsafe(no_mangle)]
pub extern "C" fn crash(kind: u32, pid: u32) {
    with(|s| {
        let (m, _) = s.machine();
        let kind = match kind {
            1 => CrashKind::Kill9(pid),
            2 => CrashKind::PowerLoss,
            3 => CrashKind::OsCrash,
            _ => return,
        };
        match kind {
            CrashKind::Kill9(pid) => {
                for tid in m.sched.live_of(pid) {
                    m.exit_thread(tid);
                }
                m.kernel.exit(pid);
                m.wake_woken();
                m.procs.remove(&pid);
                m.log(format!("kill -9 process [{pid}] (code 137)"));
            }
            CrashKind::PowerLoss | CrashKind::OsCrash => {
                let live: Vec<Tid> = m.sched.live().collect();
                for tid in live {
                    m.exit_thread(tid);
                }
                m.procs.clear();
                let step = m.sched.step();
                let t = m.clock.mono_ns();
                let mut env = Env {
                    choices: &m.choices,
                    swarm: &m.swarm,
                    trace: &mut m.trace,
                    step,
                    t_ns: t,
                    who: "crash",
                };
                let lines = m.kernel.crash(kind, &mut env);
                m.trace.push(step, t, format!("{kind:?}"));
                for l in lines {
                    m.trace.push(step, t, format!("  {l}"));
                }
            }
        }
    });
}

/// Monotonic virtual time, nanoseconds.
#[unsafe(no_mangle)]
pub extern "C" fn now_ns() -> u64 {
    with(|s| s.machine().0.clock.mono_ns())
}

/// Scheduler steps taken so far.
#[unsafe(no_mangle)]
pub extern "C" fn step() -> u64 {
    with(|s| s.machine().0.sched.step())
}

/// Render the trace into the buffer (cut to its capacity); returns the length.
#[unsafe(no_mangle)]
pub extern "C" fn trace() -> u32 {
    with(|s| {
        let text = s.machine().0.trace.render();
        s.put(text.as_bytes())
    })
}

/// Read the page-cache contents of the file whose path is `buf[0..len]` into the
/// buffer; returns the length, -1 if there is no such file (or the path is not
/// UTF-8), or -2 if it does not fit.
#[unsafe(no_mangle)]
pub extern "C" fn read_file(len: u32) -> i32 {
    with(|s| {
        let (m, buf) = s.machine();
        let Ok(path) = std::str::from_utf8(&buf[..(len as usize).min(BUF_CAP)]) else {
            return -1;
        };
        let Some(data) = m.kernel.read_file(path) else {
            return -1;
        };
        if data.len() > BUF_CAP {
            return -2;
        }
        s.put(&data) as i32
    })
}

/// Every file in the page-cache namespace as `path\tsize\tfnv1a64-hex\n` lines,
/// sorted by path, into the buffer (whole lines, cut to its capacity); returns
/// the length.
#[unsafe(no_mangle)]
pub extern "C" fn dump() -> u32 {
    with(|s| {
        let (m, _) = s.machine();
        let mut out = String::new();
        for (path, is_dir) in m.kernel.walk() {
            if is_dir {
                continue;
            }
            let data = m.kernel.read_file(&path).unwrap_or_default();
            let line = format!("{path}\t{}\t{:016x}\n", data.len(), fnv1a64(&data));
            if out.len() + line.len() > BUF_CAP {
                break;
            }
            out.push_str(&line);
        }
        s.put(out.as_bytes())
    })
}

fn fnv1a64(data: &[u8]) -> u64 {
    data.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}
