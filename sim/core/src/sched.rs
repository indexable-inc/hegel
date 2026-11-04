//! The PCT scheduler core: which simulated thread runs at each switch point.
//!
//! PCT (Burckhardt, Kothari, Musuvathi, Nagarakatte, "A Randomized Scheduler
//! with Probabilistic Guarantees of Finding Bugs", ASPLOS'10) gives each thread
//! a random priority, always runs the highest-priority runnable thread, and at
//! `d - 1` random steps lowers the running thread's priority below every initial
//! one. A bug of depth `d` is found with probability at least `1 / (n * k^(d-1))`
//! for `n` threads and `k` steps. Here every parameter is a draw made up front,
//! so a run costs a handful of draws, and the all-zero tape is the calm schedule
//! (the oldest runnable thread runs until it blocks).

use crate::{Choices, Pid};

/// A simulated thread id, unique within one simulation.
pub type Tid = u32;

/// What a blocked thread waits for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WaitKey {
    /// A mutex or rwlock, by object ordinal.
    Lock(u64),
    /// A condition variable, by object ordinal.
    Cond(u64),
    /// A thread's exit.
    Join(Tid),
    /// A thread's park token.
    Park(Tid),
    /// A channel, by object ordinal.
    Chan(u64),
    /// A thread's own timer (sleep, timed wait).
    Timer(Tid),
    /// An inode's flock state.
    Flock(u64),
    /// An inode's fcntl record locks.
    Fcntl(u64),
    /// A wasm futex: process and address.
    Futex(Pid, u64),
}

/// A thread's scheduling state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Can run.
    Runnable,
    /// Waits on a key.
    Blocked(WaitKey),
    /// Exited or killed.
    Done,
}

#[derive(Clone, Debug)]
struct Thread {
    pid: Pid,
    name: String,
    prio: i64,
    status: Status,
    yielded: bool,
}

/// The scheduler's verdict at a switch point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// Run this thread next.
    Run(Tid),
    /// Nothing is runnable but some thread is blocked.
    Idle,
    /// No live thread.
    Done,
}

/// Why the running thread lost the CPU while still runnable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Preemption {
    /// PCT change point `i` lowered its priority.
    ChangePoint(usize),
    /// A random-walk coin (swarm class `RandomPreemption`).
    Random,
    /// It yielded or spun.
    Yield,
    /// A higher-priority thread became runnable.
    Priority,
}

/// Scheduler parameters.
#[derive(Clone, Debug)]
pub struct SchedConfig {
    /// Most change points (PCT depth `d`), drawn in `0..=max_depth`.
    pub max_depth: u32,
    /// Fixed horizon in steps; when `None` the horizon is `2^e`, `e` drawn in `4..=17`.
    pub steps_hint: Option<u64>,
    /// Random-walk preemption probability per switch point, when enabled.
    pub random_walk: Option<f64>,
}

impl Default for SchedConfig {
    fn default() -> Self {
        SchedConfig {
            max_depth: 3,
            steps_hint: None,
            random_walk: None,
        }
    }
}

/// The PCT scheduler state.
#[derive(Clone, Debug)]
pub struct Sched {
    threads: Vec<Thread>,
    step: u64,
    change_points: Vec<u64>,
    next_cp: usize,
    horizon: u64,
    random_walk: Option<f64>,
    current: Option<Tid>,
    last_preemption: Option<(Tid, Tid, Preemption)>,
}

/// Lowest initial priority; change points demote below it.
const PRIO_BASE: i64 = 1 << 20;

impl Sched {
    /// Draw the depth, horizon and change points.
    pub fn new(c: &dyn Choices, cfg: &SchedConfig) -> Sched {
        let depth = c.int("sched.depth", 0, u64::from(cfg.max_depth));
        let horizon = match cfg.steps_hint {
            Some(h) => h.max(1),
            None if depth > 0 => 1u64 << c.int("sched.horizon_exp", 4, 17),
            None => 1,
        };
        let mut change_points: Vec<u64> = (0..depth)
            .map(|_| c.int("sched.change_point", 0, horizon - 1))
            .collect();
        change_points.sort_unstable();
        Sched {
            threads: Vec::new(),
            step: 0,
            change_points,
            next_cp: 0,
            horizon,
            random_walk: cfg.random_walk,
            current: None,
            last_preemption: None,
        }
    }

    /// The drawn change points, sorted.
    pub fn change_points(&self) -> &[u64] {
        &self.change_points
    }

    /// The horizon the change points were drawn over.
    pub fn horizon(&self) -> u64 {
        self.horizon
    }

    /// Steps taken so far (switch points passed).
    pub fn step(&self) -> u64 {
        self.step
    }

    /// The thread that ran last.
    pub fn current(&self) -> Option<Tid> {
        self.current
    }

    /// Register a new runnable thread of `pid` with a drawn priority.
    pub fn spawn(&mut self, c: &dyn Choices, pid: Pid, name: &str) -> Tid {
        let tid = self.threads.len() as Tid;
        let prio = PRIO_BASE + c.int("sched.priority", 0, 0xffff) as i64;
        self.threads.push(Thread {
            pid,
            name: name.to_owned(),
            prio,
            status: Status::Runnable,
            yielded: false,
        });
        tid
    }

    /// A thread's state.
    pub fn status(&self, tid: Tid) -> Status {
        self.threads[tid as usize].status
    }

    /// A thread's process.
    pub fn pid(&self, tid: Tid) -> Pid {
        self.threads[tid as usize].pid
    }

    /// A thread's name.
    pub fn name(&self, tid: Tid) -> &str {
        &self.threads[tid as usize].name
    }

    /// Every thread that is not done, in spawn order.
    pub fn live(&self) -> impl Iterator<Item = Tid> + '_ {
        self.threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.status != Status::Done)
            .map(|(i, _)| i as Tid)
    }

    /// Live threads of `pid`, in spawn order.
    pub fn live_of(&self, pid: Pid) -> Vec<Tid> {
        self.live().filter(|t| self.pid(*t) == pid).collect()
    }

    /// Block `tid` on `key`.
    pub fn block(&mut self, tid: Tid, key: WaitKey) {
        let t = &mut self.threads[tid as usize];
        if t.status != Status::Done {
            t.status = Status::Blocked(key);
        }
    }

    /// Make every thread blocked on `key` runnable; returns how many.
    pub fn wake(&mut self, key: WaitKey) -> usize {
        let mut n = 0;
        for t in &mut self.threads {
            if t.status == Status::Blocked(key) {
                t.status = Status::Runnable;
                n += 1;
            }
        }
        n
    }

    /// Make one drawn thread blocked on `key` runnable (which one is unspecified,
    /// as for `pthread_cond_signal`); returns it.
    pub fn wake_one(&mut self, c: &dyn Choices, key: WaitKey) -> Option<Tid> {
        let waiters: Vec<usize> = self
            .threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.status == Status::Blocked(key))
            .map(|(i, _)| i)
            .collect();
        if waiters.is_empty() {
            return None;
        }
        let i = c.int("sched.wake_one", 0, waiters.len() as u64 - 1) as usize;
        self.threads[waiters[i]].status = Status::Runnable;
        Some(waiters[i] as Tid)
    }

    /// Make `tid` runnable whatever it waited for.
    pub fn unblock(&mut self, tid: Tid) {
        let t = &mut self.threads[tid as usize];
        if t.status != Status::Done {
            t.status = Status::Runnable;
        }
    }

    /// `tid` exited or was killed.
    pub fn exit(&mut self, tid: Tid) {
        self.threads[tid as usize].status = Status::Done;
        if self.current == Some(tid) {
            self.current = None;
        }
    }

    /// `tid` yielded or spun: it runs last until another thread takes a step.
    pub fn yielded(&mut self, tid: Tid) {
        self.threads[tid as usize].yielded = true;
    }

    /// Why the last pick switched away from a runnable thread, if it did.
    pub fn last_preemption(&self) -> Option<(Tid, Tid, Preemption)> {
        self.last_preemption
    }

    /// Advance one step and choose the next thread to run.
    pub fn pick(&mut self, c: &dyn Choices) -> Pick {
        self.step += 1;
        self.last_preemption = None;
        let mut reason = None;
        if let Some(cur) = self.current {
            let running = self.threads[cur as usize].status == Status::Runnable;
            while self.next_cp < self.change_points.len()
                && self.step > self.change_points[self.next_cp]
            {
                if running {
                    let low = -((self.change_points.len() - self.next_cp) as i64);
                    self.threads[cur as usize].prio = low;
                    reason = Some(Preemption::ChangePoint(self.next_cp));
                }
                self.next_cp += 1;
            }
            if running && self.threads[cur as usize].yielded && reason.is_none() {
                reason = Some(Preemption::Yield);
            }
        }
        let runnable: Vec<Tid> = self
            .threads
            .iter()
            .enumerate()
            .filter(|(_, t)| t.status == Status::Runnable)
            .map(|(i, _)| i as Tid)
            .collect();
        if runnable.is_empty() {
            return if self.live().next().is_some() {
                Pick::Idle
            } else {
                Pick::Done
            };
        }
        let mut chosen = None;
        if let Some(p) = self.random_walk
            && runnable.len() > 1
            && c.coin("sched.random_preempt", p)
        {
            let i = c.int("sched.random_thread", 0, runnable.len() as u64 - 1) as usize;
            chosen = Some(runnable[i]);
            reason = Some(Preemption::Random);
        }
        let chosen = chosen.unwrap_or_else(|| {
            *runnable
                .iter()
                .max_by_key(|&&t| {
                    let th = &self.threads[t as usize];
                    (!th.yielded, th.prio, std::cmp::Reverse(t))
                })
                .unwrap_or(&runnable[0])
        });
        if let Some(cur) = self.current
            && cur != chosen
        {
            if self.threads[cur as usize].status == Status::Runnable {
                self.last_preemption =
                    Some((cur, chosen, reason.unwrap_or(Preemption::Priority)));
            }
            for (i, t) in self.threads.iter_mut().enumerate() {
                if i as Tid != chosen {
                    t.yielded = false;
                }
            }
        }
        self.current = Some(chosen);
        Pick::Run(chosen)
    }
}
