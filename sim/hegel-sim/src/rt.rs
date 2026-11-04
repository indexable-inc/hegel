//! The native driver: simulated threads as corosensei fibers on one OS thread,
//! a switch point at every facade operation, and the loop that applies each
//! request to the sans-IO core.
//!
//! A fiber never decides anything. At a switch point it suspends with a
//! [`Req`] and the driver loop, on the test's own stack, applies it to the
//! kernel, scheduler and clock, draws every choice, and resumes whichever thread
//! the scheduler picks. So every draw and every kernel state change happens on
//! the driver's stack.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;

use corosensei::stack::DefaultStack;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use hegel_sim_core::clock::TimerId;
use hegel_sim_core::kernel::{CrashKind, Env, Kernel};
use hegel_sim_core::sched::{Pick, Preemption, SchedConfig, Status};
use hegel_sim_core::trace::fmt_ns;
use hegel_sim_core::{Choices, Clock, Fault, Pid, Sched, SysResult, Syscall, Tid, Trace, WaitKey};

use crate::sim::{Config, CrashPoints, Exit, Outcome};

/// A request a fiber makes at a switch point.
pub(crate) enum Req {
    /// About to perform an operation on an object (atomic, lock, ...).
    Op(&'static str, usize),
    /// Run a syscall.
    Syscall(Syscall),
    /// Block until `key` is woken; the caller re-checks its condition.
    Block(WaitKey),
    /// Block until `key` is woken or monotonic time reaches the deadline.
    TimedBlock(WaitKey, u64),
    /// Sleep until the deadline (monotonic ns).
    Sleep(u64),
    /// Start a thread in the caller's process.
    Spawn(String, Box<dyn FnOnce()>),
    /// The caller yields or spins.
    Yield,
    /// The process exits with this code.
    ExitProcess(i32),
    /// Send a signal to a process (`kill(2)`).
    Kill(Pid, i32),
}

/// What a fiber is resumed with.
pub(crate) enum Reply {
    None,
    Sys(SysResult),
    Spawned(Tid),
    /// Run this signal handler, then carry on with the inner reply.
    Signal(i32, fn(i32), Box<Reply>),
}

type Fiber = Coroutine<Reply, Req, (), DefaultStack>;

pub(crate) struct ThreadInfo {
    pub(crate) pid: Pid,
    pub(crate) name: String,
    yielder: *const Yielder<Reply, Req>,
    reply: Option<Reply>,
    retry: Option<Syscall>,
    timer: Option<TimerId>,
    pub(crate) park_token: bool,
    last_op: (&'static str, usize),
    spin: (usize, u64, u32),
    in_handler: bool,
}

/// A signal handler and whether it restarts interrupted syscalls.
pub(crate) type Handler = (fn(i32), bool);

pub(crate) struct ProcInfo {
    pub(crate) name: String,
    pub(crate) main: Tid,
    pub(crate) code: Option<i32>,
    pub(crate) alive: bool,
    pub(crate) handlers: BTreeMap<i32, Handler>,
    pub(crate) pending: Vec<i32>,
}

pub(crate) struct World {
    choices: *const (dyn Choices + 'static),
    pub(crate) cfg: Config,
    pub(crate) kernel: Kernel,
    pub(crate) sched: Sched,
    pub(crate) clock: Clock,
    pub(crate) swarm: hegel_sim_core::Swarm,
    pub(crate) trace: Trace,
    pub(crate) threads: Vec<ThreadInfo>,
    pub(crate) procs: BTreeMap<Pid, ProcInfo>,
    next_pid: Pid,
    objects: BTreeMap<usize, u64>,
    pub(crate) crashes_left: u32,
    crash_rate: f64,
    panicked: Option<(Tid, Box<dyn Any + Send>)>,
    pub(crate) current: Option<Tid>,
    pub(crate) tls: BTreeMap<(Tid, usize), Rc<dyn Any>>,
    pub(crate) sometimes: BTreeMap<&'static str, (u64, u64)>,
    exited: Vec<Exit>,
}

impl World {
    pub(crate) fn choices(&self) -> &'static dyn Choices {
        // SAFETY: `Sim` holds the `&dyn Choices` this was made from for its whole
        // life, and the world lives inside the `Sim`.
        unsafe { &*self.choices }
    }

    /// A stable small number for the object at `addr`, by first touch.
    pub(crate) fn object(&mut self, addr: usize) -> u64 {
        let n = self.objects.len() as u64;
        *self.objects.entry(addr).or_insert(n)
    }

    fn label(&self, tid: Tid) -> String {
        let t = &self.threads[tid as usize];
        let p = self.procs.get(&t.pid).map(|p| p.name.as_str()).unwrap_or("?");
        format!("{p}[{}].{}", t.pid, t.name)
    }

    fn log(&mut self, text: String) {
        let step = self.sched.step();
        let t = self.clock.mono_ns();
        self.trace.push(step, t, text);
    }
}

pub(crate) struct Runtime {
    pub(crate) world: RefCell<World>,
    fibers: RefCell<Vec<Option<Fiber>>>,
    in_fiber: Cell<bool>,
}

thread_local! {
    static RT: Cell<*const Runtime> = const { Cell::new(std::ptr::null()) };
}

fn runtime() -> Option<&'static Runtime> {
    let p = RT.with(Cell::get);
    // SAFETY: `RT` is set only while `Sim::run` borrows the runtime and is
    // cleared before it returns; fibers run only inside `run`.
    (!p.is_null()).then(|| unsafe { &*p })
}

/// Whether the caller is a simulated thread inside a running simulation.
pub(crate) fn in_sim() -> bool {
    runtime().is_some_and(|r| r.in_fiber.get())
}

/// Run `f` on the world. Panics outside a simulation.
pub(crate) fn with<R>(f: impl FnOnce(&mut World) -> R) -> R {
    let rt = runtime().unwrap_or_else(|| panic!("hegel-sim: a simulated operation ran outside a running simulation"));
    f(&mut rt.world.borrow_mut())
}

/// Suspend the calling fiber with `req` and return what it is resumed with.
pub(crate) fn request(req: Req) -> Reply {
    let y = with(|w| {
        let tid = w.current.unwrap_or_else(|| panic!("hegel-sim: no current simulated thread"));
        w.threads[tid as usize].yielder
    });
    // SAFETY: the yielder belongs to the running fiber (this one), which stays
    // alive while it is suspended; no world borrow is held across the suspend.
    let mut reply = unsafe { (*y).suspend(req) };
    while let Reply::Signal(sig, handler, inner) = reply {
        with(|w| {
            let tid = w.current.unwrap_or_default();
            w.threads[tid as usize].in_handler = true;
        });
        handler(sig);
        with(|w| {
            let tid = w.current.unwrap_or_default();
            w.threads[tid as usize].in_handler = false;
        });
        reply = *inner;
    }
    reply
}

/// A switch point before an operation on the object at `addr`.
pub(crate) fn op(what: &'static str, addr: usize) {
    if in_sim() {
        request(Req::Op(what, addr));
    }
}

/// After an atomic load: the same value 32 times in a row on one thread counts
/// as spinning, which yields (shuttle's PCT rule), so a spinner cannot livelock
/// a lower-priority thread it waits for.
pub(crate) fn after_load(addr: usize, value: u64) {
    if !in_sim() {
        return;
    }
    with(|w| {
        let Some(tid) = w.current else { return };
        let t = &mut w.threads[tid as usize];
        if t.spin.0 == addr && t.spin.1 == value {
            t.spin.2 += 1;
            if t.spin.2 >= 32 {
                t.spin.2 = 0;
                w.sched.yielded(tid);
            }
        } else {
            t.spin = (addr, value, 1);
        }
    });
}

/// Run a syscall in the simulated kernel.
pub(crate) fn syscall(call: Syscall) -> SysResult {
    match request(Req::Syscall(call)) {
        Reply::Sys(r) => r,
        _ => panic!("hegel-sim: a syscall was resumed without its result"),
    }
}

/// Wake every thread blocked on `key`.
pub(crate) fn wake(key: WaitKey) {
    with(|w| {
        w.sched.wake(key);
    });
}

/// The current simulated thread.
pub(crate) fn current_tid() -> Tid {
    with(|w| w.current.unwrap_or_else(|| panic!("hegel-sim: no current simulated thread")))
}

/// The current simulated process.
pub(crate) fn current_pid() -> Pid {
    with(|w| w.threads[w.current.unwrap_or_default() as usize].pid)
}

/// Record a `sometimes` observation.
pub(crate) fn sometimes(name: &'static str, cond: bool) {
    if let Some(rt) = runtime() {
        let mut w = rt.world.borrow_mut();
        let e = w.sometimes.entry(name).or_insert((0, 0));
        e.0 += 1;
        e.1 += u64::from(cond);
    }
}

pub(crate) fn record_panic(payload: Box<dyn Any + Send>) {
    with(|w| {
        let tid = w.current.unwrap_or_default();
        if w.panicked.is_none() {
            w.panicked = Some((tid, payload));
        }
    });
}

pub(crate) struct Driver {
    rt: Box<Runtime>,
}

struct Installed;

impl Installed {
    fn new(rt: &Runtime) -> Installed {
        RT.with(|c| c.set(rt));
        Installed
    }
}

impl Drop for Installed {
    fn drop(&mut self) {
        RT.with(|c| c.set(std::ptr::null()));
    }
}

fn panic_message(p: &(dyn Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "(non-string panic payload)".to_owned()
    }
}

impl Driver {
    pub(crate) fn new(choices: &dyn Choices, cfg: Config) -> Driver {
        // SAFETY: erases the lifetime of the choice port; `Sim<'a>` keeps the
        // `&'a dyn Choices` alive for as long as this driver exists.
        let choices: *const (dyn Choices + 'static) = unsafe { std::mem::transmute::<*const dyn Choices, *const (dyn Choices + 'static)>(choices as *const dyn Choices) };
        // SAFETY: just made from a live reference.
        let c = unsafe { &*choices };
        let swarm = if cfg.swarm {
            hegel_sim_core::Swarm::draw(c, cfg.faults, cfg.rates.clone())
        } else {
            hegel_sim_core::Swarm::fixed(cfg.faults, cfg.rates.clone())
        };
        let sched = Sched::new(
            c,
            &SchedConfig {
                max_depth: cfg.max_depth,
                steps_hint: cfg.steps_hint,
                random_walk: swarm.on(Fault::RandomPreemption).then_some(swarm.rates.random_preemption),
            },
        );
        let mut trace = Trace::new();
        let classes: Vec<String> = swarm.enabled.iter().map(|f| format!("{f:?}")).collect();
        trace.push(0, 0, format!("swarm: {}", if classes.is_empty() { "no fault classes".into() } else { classes.join(", ") }));
        if !sched.change_points().is_empty() {
            trace.push(0, 0, format!("pct: change points {:?} over horizon {}", sched.change_points(), sched.horizon()));
        }
        let world = World {
            choices,
            kernel: Kernel::new(cfg.kernel.clone()),
            crashes_left: cfg.max_crashes,
            cfg,
            sched,
            clock: Clock::new(),
            swarm,
            trace,
            threads: Vec::new(),
            procs: BTreeMap::new(),
            next_pid: 1,
            objects: BTreeMap::new(),
            crash_rate: 0.0,
            panicked: None,
            current: None,
            tls: BTreeMap::new(),
            sometimes: BTreeMap::new(),
            exited: Vec::new(),
        };
        Driver {
            rt: Box::new(Runtime {
                world: RefCell::new(world),
                fibers: RefCell::new(Vec::new()),
                in_fiber: Cell::new(false),
            }),
        }
    }

    /// Answer every later choice from `choices` instead (a forked branch must not
    /// talk to the parent's test case).
    ///
    /// # Safety
    /// `choices` must outlive every later use of this driver.
    pub(crate) unsafe fn set_choices(&mut self, choices: &dyn Choices) {
        // SAFETY: the caller keeps `choices` alive; see `Driver::new`.
        self.rt.world.borrow_mut().choices = unsafe { std::mem::transmute::<*const dyn Choices, *const (dyn Choices + 'static)>(choices as *const dyn Choices) };
    }

    pub(crate) fn world(&self) -> std::cell::RefMut<'_, World> {
        self.rt.world.borrow_mut()
    }

    fn make_fiber(stack_size: usize, tid: Tid, body: Box<dyn FnOnce()>) -> Fiber {
        let stack = DefaultStack::new(stack_size).unwrap_or_else(|e| panic!("hegel-sim: cannot allocate a {stack_size}-byte fiber stack: {e}"));
        Coroutine::with_stack(stack, move |y: &Yielder<Reply, Req>, _first: Reply| {
            with(|w| w.threads[tid as usize].yielder = y as *const _);
            if let Err(p) = catch_unwind(AssertUnwindSafe(body)) {
                record_panic(p);
            }
        })
    }

    fn add_thread(&mut self, pid: Pid, name: String, body: Box<dyn FnOnce()>) -> Tid {
        let (tid, stack) = {
            let mut w = self.rt.world.borrow_mut();
            let c = w.choices;
            // SAFETY: see `World::choices`.
            let tid = w.sched.spawn(unsafe { &*c }, pid, &name);
            w.threads.push(ThreadInfo {
                pid,
                name,
                yielder: std::ptr::null(),
                reply: None,
                retry: None,
                timer: None,
                park_token: false,
                last_op: ("start", 0),
                spin: (0, 0, 0),
                in_handler: false,
            });
            (tid, w.cfg.stack_size)
        };
        let fiber = Self::make_fiber(stack, tid, body);
        let mut fibers = self.rt.fibers.borrow_mut();
        if fibers.len() <= tid as usize {
            fibers.resize_with(tid as usize + 1, || None);
        }
        fibers[tid as usize] = Some(fiber);
        tid
    }

    pub(crate) fn spawn_process(&mut self, name: &str, program: Box<dyn FnOnce() -> i32>) -> Pid {
        let pid = {
            let mut w = self.rt.world.borrow_mut();
            let pid = w.next_pid;
            w.next_pid += 1;
            w.kernel.spawn(pid);
            w.procs.insert(
                pid,
                ProcInfo { name: name.to_owned(), main: 0, code: None, alive: true, handlers: BTreeMap::new(), pending: Vec::new() },
            );
            w.log(format!("spawn process {name}[{pid}]"));
            pid
        };
        let body: Box<dyn FnOnce()> = Box::new(move || {
            let code = program();
            with(|w| {
                if let Some(p) = w.procs.get_mut(&pid) {
                    p.code = Some(code);
                }
            });
        });
        let tid = self.add_thread(pid, "main".into(), body);
        if let Some(p) = self.rt.world.borrow_mut().procs.get_mut(&pid) {
            p.main = tid;
        }
        pid
    }

    /// Kill every fiber of `pid` without running destructors, as SIGKILL does.
    fn kill_fibers(&mut self, pid: Pid) {
        let tids = self.rt.world.borrow().sched.live_of(pid);
        for tid in tids {
            if let Some(mut f) = self.rt.fibers.borrow_mut()[tid as usize].take() {
                // SAFETY: nothing outside this process holds a pointer into its
                // fiber stacks: the kernel stores no user pointers, wait queues hold
                // thread ids, programs are 'static and processes share no Rust
                // state except ghost oracle state, which never points into a
                // stack. Skipped destructors only leak.
                unsafe { f.force_reset() };
                drop(f);
            }
            let mut w = self.rt.world.borrow_mut();
            w.sched.exit(tid);
            w.sched.wake(WaitKey::Join(tid));
            let keys: Vec<(Tid, usize)> = w.tls.range((tid, 0)..=(tid, usize::MAX)).map(|(k, _)| *k).collect();
            for k in keys {
                if let Some(v) = w.tls.remove(&k) {
                    std::mem::forget(v);
                }
            }
        }
    }

    fn end_process(&mut self, pid: Pid, code: i32, how: &str) {
        self.kill_fibers(pid);
        let mut w = self.rt.world.borrow_mut();
        w.kernel.exit(pid);
        for k in w.kernel.take_woken() {
            w.sched.wake(k);
        }
        let name = match w.procs.get_mut(&pid) {
            Some(p) if p.alive => {
                p.alive = false;
                p.code = Some(code);
                p.name.clone()
            }
            _ => return,
        };
        w.log(format!("{how} process {name}[{pid}] (code {code})"));
        w.exited.push(Exit { pid, name, code });
    }

    pub(crate) fn kill(&mut self, pid: Pid) {
        self.end_process(pid, 128 + 9, "kill -9");
    }

    pub(crate) fn crash(&mut self, kind: CrashKind) {
        match kind {
            CrashKind::Kill9(pid) => self.kill(pid),
            CrashKind::PowerLoss | CrashKind::OsCrash => {
                let pids: Vec<Pid> = self.rt.world.borrow().procs.iter().filter(|(_, p)| p.alive).map(|(p, _)| *p).collect();
                for pid in pids {
                    self.kill_fibers(pid);
                    let mut w = self.rt.world.borrow_mut();
                    if let Some(p) = w.procs.get_mut(&pid) {
                        p.alive = false;
                        p.code = Some(128 + 9);
                    }
                }
                let mut w = self.rt.world.borrow_mut();
                let w = &mut *w;
                let step = w.sched.step();
                let t = w.clock.mono_ns();
                // SAFETY: see `World::choices`.
                let choices = unsafe { &*w.choices };
                let mut env = Env { choices, swarm: &w.swarm, trace: &mut w.trace, step, t_ns: t, who: "crash" };
                let lines = w.kernel.crash(kind, &mut env);
                w.trace.push(step, t, format!("{kind:?}"));
                for l in lines {
                    w.trace.push(step, t, format!("  {l}"));
                }
                w.threads.iter_mut().for_each(|t| t.reply = None);
            }
        }
    }

    /// Draw whether to crash here; returns the crash to perform.
    fn maybe_crash(w: &mut World) -> Option<CrashKind> {
        if w.crashes_left == 0 || w.crash_rate <= 0.0 {
            return None;
        }
        let c = w.choices();
        if !c.coin("crash.now", w.crash_rate) {
            return None;
        }
        let kinds: Vec<Fault> = Fault::CRASHES.into_iter().filter(|f| w.swarm.on(*f)).collect();
        let k = kinds[c.int("crash.kind", 0, kinds.len() as u64 - 1) as usize];
        let kind = match k {
            Fault::Kill9 => {
                let live: Vec<Pid> = w.procs.iter().filter(|(_, p)| p.alive).map(|(p, _)| *p).collect();
                CrashKind::Kill9(live[c.int("crash.victim", 0, live.len() as u64 - 1) as usize])
            }
            Fault::OsCrash => CrashKind::OsCrash,
            _ => CrashKind::PowerLoss,
        };
        w.crashes_left -= 1;
        Some(kind)
    }

    fn deliver_signal(w: &mut World, pid: Pid, sig: i32) -> Option<Pid> {
        let p = w.procs.get_mut(&pid)?;
        if !p.alive {
            return None;
        }
        if sig != 9 && p.handlers.contains_key(&sig) {
            p.pending.push(sig);
            // A thread blocked in an interruptible wait takes it now.
            for tid in w.sched.live_of(pid) {
                if let Status::Blocked(WaitKey::Flock(_) | WaitKey::Fcntl(_) | WaitKey::Timer(_)) = w.sched.status(tid) {
                    w.sched.unblock(tid);
                    break;
                }
            }
            None
        } else if sig == 9 || matches!(sig, 2 | 15) {
            // signal(7): the default action of SIGINT and SIGTERM is to terminate.
            Some(pid)
        } else {
            None
        }
    }

    /// Run until every process has exited or a crash fires.
    pub(crate) fn run(&mut self) -> Outcome {
        let _installed = Installed::new(&self.rt);
        {
            let mut w = self.rt.world.borrow_mut();
            let crash_classes = Fault::CRASHES.iter().any(|f| w.swarm.on(*f));
            w.crash_rate = if w.crashes_left > 0 && crash_classes {
                let e = w.choices().int("crash.rate_exp", 1, 11);
                1.0 / (1u64 << e) as f64
            } else {
                0.0
            };
            w.exited.clear();
        }
        loop {
            let pick = {
                let mut w = self.rt.world.borrow_mut();
                let w = &mut *w;
                for k in w.clock.fire_due() {
                    if let WaitKey::Timer(t) = k {
                        w.sched.unblock(t);
                    }
                }
                let c = w.choices();
                let pick = w.sched.pick(c);
                if let Some((from, to, why)) = w.sched.last_preemption() {
                    let (op, obj) = w.threads[from as usize].last_op;
                    let why = match why {
                        Preemption::ChangePoint(i) => format!("change point {i}"),
                        Preemption::Random => "random walk".into(),
                        Preemption::Yield => "yield".into(),
                        Preemption::Priority => "priority".into(),
                    };
                    let text = format!("preempt {} before {op}#{} -> {} ({why})", w.label(from), w.object(obj), w.label(to));
                    w.log(text);
                }
                if w.sched.step() > w.cfg.max_steps {
                    let cur = w.sched.current().map(|t| w.label(t)).unwrap_or_default();
                    panic!(
                        "hegel-sim: the step bound ({}) was reached; {cur} ran last. A livelock or a loop with no bound.\n{}",
                        w.cfg.max_steps,
                        w.trace.render()
                    );
                }
                pick
            };
            let tid = match pick {
                Pick::Done => {
                    let mut w = self.rt.world.borrow_mut();
                    return Outcome::Exited(std::mem::take(&mut w.exited));
                }
                Pick::Idle => {
                    let mut w = self.rt.world.borrow_mut();
                    if w.clock.jump_to_next() {
                        continue;
                    }
                    let blocked: Vec<String> = w
                        .sched
                        .live()
                        .collect::<Vec<_>>()
                        .into_iter()
                        .map(|t| format!("{} waits on {:?}", w.label(t), w.sched.status(t)))
                        .collect();
                    panic!("hegel-sim: deadlock: every live thread is blocked and no timer is pending:\n  {}\n{}", blocked.join("\n  "), w.trace.render());
                }
                Pick::Run(t) => t,
            };
            // A thread retrying a blocked syscall retries it before it resumes.
            let retry = {
                let mut w = self.rt.world.borrow_mut();
                let tick = w.cfg.tick_ns;
                w.clock.advance(tick);
                if let Some(id) = w.threads[tid as usize].timer.take() {
                    w.clock.cancel(id);
                }
                w.threads[tid as usize].retry.take()
            };
            if let Some(call) = retry {
                if let Some(out) = self.handle(tid, Req::Syscall(call)) {
                    return out;
                }
                if self.rt.world.borrow().threads[tid as usize].reply.is_none() {
                    continue;
                }
            }
            let reply = {
                let mut w = self.rt.world.borrow_mut();
                let pid = w.threads[tid as usize].pid;
                let reply = w.threads[tid as usize].reply.take().unwrap_or(Reply::None);
                let in_handler = w.threads[tid as usize].in_handler;
                match w.procs.get_mut(&pid) {
                    Some(p) if !in_handler && !p.pending.is_empty() => {
                        let sig = p.pending.remove(0);
                        let (h, restart) = p.handlers[&sig];
                        let reply = match reply {
                            Reply::Sys(SysResult::Block(_)) if !restart => Reply::Sys(SysResult::Err(hegel_sim_core::Errno::EINTR)),
                            r => r,
                        };
                        let text = format!("signal {sig} handled by {}", w.label(tid));
                        w.log(text);
                        Reply::Signal(sig, h, Box::new(reply))
                    }
                    _ => reply,
                }
            };
            let result = {
                let mut fiber = match self.rt.fibers.borrow_mut()[tid as usize].take() {
                    Some(f) => f,
                    None => continue,
                };
                self.rt.world.borrow_mut().current = Some(tid);
                self.rt.in_fiber.set(true);
                let r = fiber.resume(reply);
                self.rt.in_fiber.set(false);
                self.rt.fibers.borrow_mut()[tid as usize] = Some(fiber);
                r
            };
            if let Some((ptid, payload)) = self.rt.world.borrow_mut().panicked.take() {
                let w = self.rt.world.borrow();
                let who = w.label(ptid);
                let msg = panic_message(&*payload);
                w.choices().note(&format!("hegel-sim: {who} panicked: {msg}"));
                drop(w);
                std::panic::resume_unwind(payload);
            }
            match result {
                CoroutineResult::Yield(req) => {
                    if let Some(out) = self.handle(tid, req) {
                        return out;
                    }
                }
                CoroutineResult::Return(()) => {
                    self.rt.fibers.borrow_mut()[tid as usize] = None;
                    let main_of = {
                        let mut w = self.rt.world.borrow_mut();
                        w.sched.exit(tid);
                        w.sched.wake(WaitKey::Join(tid));
                        let pid = w.threads[tid as usize].pid;
                        let keys: Vec<(Tid, usize)> = w.tls.range((tid, 0)..=(tid, usize::MAX)).map(|(k, _)| *k).collect();
                        let vals: Vec<Rc<dyn Any>> = keys.iter().filter_map(|k| w.tls.remove(k)).collect();
                        drop(w);
                        drop(vals);
                        let w = self.rt.world.borrow();
                        w.procs.get(&pid).filter(|p| p.main == tid).map(|p| (pid, p.code.unwrap_or(0)))
                    };
                    if let Some((pid, code)) = main_of {
                        self.end_process(pid, code, "exit");
                    }
                }
            }
        }
    }

    /// Apply a request from `tid`; returns an outcome when the run must stop.
    fn handle(&mut self, tid: Tid, req: Req) -> Option<Outcome> {
        let mut w = self.rt.world.borrow_mut();
        match req {
            Req::Op(what, addr) => {
                w.threads[tid as usize].last_op = (what, addr);
                if w.cfg.crash_points == CrashPoints::EverySwitch
                    && let Some(kind) = Self::maybe_crash(&mut w)
                {
                    drop(w);
                    self.crash(kind);
                    return Some(Outcome::Crashed(kind));
                }
            }
            Req::Syscall(call) => {
                w.threads[tid as usize].last_op = (call.name(), 0);
                if let Some(kind) = Self::maybe_crash(&mut w) {
                    drop(w);
                    self.crash(kind);
                    return Some(Outcome::Crashed(kind));
                }
                let term = {
                    let w = &mut *w;
                    let c = w.choices();
                    if w.swarm.fire(c, Fault::AsyncSignals, w.swarm.rates.async_signal) {
                        let live: Vec<Pid> = w.procs.iter().filter(|(_, p)| p.alive).map(|(p, _)| *p).collect();
                        let pid = live[c.int("signal.victim", 0, live.len() as u64 - 1) as usize];
                        w.log(format!("async SIGTERM to process {pid}"));
                        Self::deliver_signal(w, pid, 15)
                    } else {
                        None
                    }
                };
                if let Some(p) = term {
                    let me = w.threads[tid as usize].pid;
                    drop(w);
                    self.end_process(p, 128 + 15, "SIGTERM");
                    if p == me {
                        return None;
                    }
                    w = self.rt.world.borrow_mut();
                }
                let w = &mut *w;
                let c = w.choices();
                if w.swarm.fire(c, Fault::ClockJumps, w.swarm.rates.clock_jump) {
                    let mag = c.int("clock.jump_secs", 1, 3600) as i128 * 1_000_000_000;
                    let delta = if c.coin("clock.jump_back", 0.5) { -mag } else { mag };
                    w.clock.jump_realtime(delta);
                    w.log(format!("realtime clock jumps {}{}", if delta < 0 { "-" } else { "+" }, fmt_ns((delta.unsigned_abs()) as u64)));
                }
                let pid = w.threads[tid as usize].pid;
                let who = w.label(tid);
                let step = w.sched.step();
                let t = w.clock.mono_ns();
                let mut env = Env { choices: c, swarm: &w.swarm, trace: &mut w.trace, step, t_ns: t, who: &who };
                let r = w.kernel.syscall(pid, &call, &mut env);
                w.clock.advance(call.cost_ns());
                for k in w.kernel.take_woken() {
                    w.sched.wake(k);
                }
                match r {
                    SysResult::Block(key) => {
                        w.sched.block(tid, key);
                        w.threads[tid as usize].retry = Some(call);
                    }
                    r => w.threads[tid as usize].reply = Some(Reply::Sys(r)),
                }
            }
            Req::Block(key) => {
                let c = w.choices();
                let spurious = matches!(key, WaitKey::Cond(_)) && w.swarm.fire(c, Fault::SpuriousWakeups, w.swarm.rates.spurious_wakeup);
                if spurious {
                    let text = format!("spurious wakeup of {}", w.label(tid));
                    w.log(text);
                } else {
                    w.sched.block(tid, key);
                }
            }
            Req::TimedBlock(key, deadline) => {
                let deadline = Self::lateness(&mut w, deadline);
                let id = w.clock.add_timer(deadline, WaitKey::Timer(tid));
                w.threads[tid as usize].timer = Some(id);
                w.sched.block(tid, key);
            }
            Req::Sleep(deadline) => {
                let deadline = Self::lateness(&mut w, deadline);
                let id = w.clock.add_timer(deadline, WaitKey::Timer(tid));
                w.threads[tid as usize].timer = Some(id);
                w.sched.block(tid, WaitKey::Timer(tid));
            }
            Req::Spawn(name, body) => {
                let pid = w.threads[tid as usize].pid;
                let label = format!("spawn thread {name} in {}", w.label(tid));
                w.log(label);
                drop(w);
                let new = self.add_thread(pid, name, body);
                self.rt.world.borrow_mut().threads[tid as usize].reply = Some(Reply::Spawned(new));
            }
            Req::Yield => w.sched.yielded(tid),
            Req::ExitProcess(code) => {
                let pid = w.threads[tid as usize].pid;
                drop(w);
                self.end_process(pid, code, "exit");
            }
            Req::Kill(pid, sig) => {
                let text = format!("{} sends signal {sig} to process {pid}", w.label(tid));
                w.log(text);
                let term = Self::deliver_signal(&mut w, pid, sig);
                drop(w);
                if let Some(p) = term {
                    self.end_process(p, 128 + sig, "signal");
                }
            }
        }
        None
    }

    /// nanosleep(2): "the sleep may be lengthened slightly"; never shortened.
    fn lateness(w: &mut World, deadline: u64) -> u64 {
        let c = w.choices();
        if w.swarm.fire(c, Fault::LateWakeups, w.swarm.rates.late_wakeup) {
            deadline + c.int("clock.late_ns", 1, 10_000_000)
        } else {
            deadline
        }
    }

    pub(crate) fn send_signal(&mut self, pid: Pid, sig: i32) {
        let term = {
            let mut w = self.rt.world.borrow_mut();
            w.log(format!("signal {sig} to process {pid}"));
            Self::deliver_signal(&mut w, pid, sig)
        };
        if let Some(p) = term {
            self.end_process(p, 128 + sig, "signal");
        }
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        let mut fibers = self.rt.fibers.borrow_mut();
        for f in fibers.iter_mut() {
            if let Some(mut f) = f.take() {
                // SAFETY: as in `kill_fibers`: the whole simulated machine goes away.
                unsafe { f.force_reset() };
            }
        }
        let mut w = self.rt.world.borrow_mut();
        for (_, v) in std::mem::take(&mut w.tls) {
            std::mem::forget(v);
        }
    }
}
