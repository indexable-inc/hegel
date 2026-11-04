//! A simulated world in wasm: one machine instance (kernel, scheduler, clock,
//! all in its linear memory) and one instance per process, each process's
//! threads switched with Asyncify. The host only routes calls, answers draws
//! and keeps a few serializable tables.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use hegel_sim_core::kernel::{Errno, OpenFlags, SysOk, Whence};
use hegel_sim_core::{Choices, FaultSet, SysResult, Syscall};
use serde::{Deserialize, Serialize};
use wasmtime::{
    Caller, Config, Engine, Extern, Func, Instance, Linker, Memory, Module, Store, TypedFunc, Val,
};

use crate::instrument;
use crate::machine::{Crash, Machine};

/// How a process's suspended thread stacks are kept in memory the snapshot
/// covers. Chosen per world; there is no fallback between modes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum StackMode {
    /// Binaryen Asyncify: stacks unwound into linear memory (phase 2a).
    #[default]
    Asyncify,
    /// Stack-switching continuations from the `indexable-inc/wasmtime` fork,
    /// stacks in a snapshot-owned region (phase 2b).
    Continuations,
}

/// World parameters (the machine's, plus the host's).
#[derive(Clone, Debug)]
pub struct WasmConfig {
    /// 0 LinuxStrict, 1 LinuxExt4, 2 MacOs.
    pub profile: u8,
    /// Disk capacity in bytes.
    pub capacity: u64,
    /// Device atomic write unit in bytes.
    pub atomic_write_unit: u64,
    /// Most PCT change points.
    pub max_depth: u32,
    /// Draw which fault classes are on.
    pub swarm: bool,
    /// Allowed fault classes.
    pub faults: FaultSet,
    /// Most crashes over the world's life.
    pub max_crashes: u32,
    /// Virtual nanoseconds per step.
    pub tick_ns: u64,
    /// How thread stacks are kept.
    pub stacks: StackMode,
}

impl Default for WasmConfig {
    fn default() -> Self {
        WasmConfig {
            profile: 0,
            capacity: 1 << 30,
            atomic_write_unit: 512,
            max_depth: 3,
            swarm: true,
            faults: FaultSet::all(),
            max_crashes: 2,
            tick_ns: 1_000,
            stacks: StackMode::Asyncify,
        }
    }
}

impl WasmConfig {
    /// Allow exactly these fault classes.
    pub fn faults(mut self, f: impl IntoIterator<Item = hegel_sim_core::Fault>) -> Self {
        self.faults = f.into_iter().collect();
        self
    }

    /// Most crashes over the world's life.
    pub fn max_crashes(mut self, n: u32) -> Self {
        self.max_crashes = n;
        self
    }

    fn encode(&self) -> Vec<u8> {
        let mut b = vec![self.profile];
        b.extend_from_slice(&self.capacity.to_le_bytes());
        b.extend_from_slice(&self.atomic_write_unit.to_le_bytes());
        b.extend_from_slice(&self.max_depth.to_le_bytes());
        b.push(u8::from(self.swarm));
        let mask: u32 = hegel_sim_core::Fault::ALL
            .iter()
            .enumerate()
            .filter(|(_, f)| self.faults.has(**f))
            .map(|(i, _)| 1u32 << i)
            .sum();
        b.extend_from_slice(&mask.to_le_bytes());
        b.extend_from_slice(&self.max_crashes.to_le_bytes());
        b.extend_from_slice(&self.tick_ns.to_le_bytes());
        b
    }
}

/// A compiled machine module: `hegel-sim-machine`'s wasm with its globals
/// exported.
#[derive(Clone)]
pub struct MachineModule {
    pub(crate) module: Module,
    pub(crate) hash: [u8; 32],
}

impl MachineModule {
    /// Compile the machine module from its wasm bytes.
    pub fn new(engine: &Engine, wasm: &[u8]) -> Result<MachineModule> {
        let bytes = instrument::export_globals(wasm)?;
        Ok(MachineModule {
            module: Module::new(engine, &bytes)?,
            hash: *blake3::hash(&bytes).as_bytes(),
        })
    }
}

/// A program (one version of one binary), instrumented and compiled.
#[derive(Clone)]
pub struct Program {
    /// Its name in traces.
    pub name: String,
    pub(crate) module: Module,
    pub(crate) hash: [u8; 32],
    /// Atomic instructions that got a switch point.
    pub atomics: usize,
    /// Size of the prepared module in bytes.
    pub prepared_len: usize,
}

impl Program {
    /// Instrument (yields, Asyncify, global exports) and compile a
    /// `wasm32-wasip1` program.
    pub fn new(engine: &Engine, name: &str, wasm: &[u8]) -> Result<Program> {
        let (bytes, atomics) = instrument::prepare_program(wasm)?;
        Ok(Program {
            name: name.to_owned(),
            module: Module::new(engine, &bytes)
                .with_context(|| format!("hegel-sim-wasm: compiling program {name}"))?,
            hash: *blake3::hash(&bytes).as_bytes(),
            atomics,
            prepared_len: bytes.len(),
        })
    }
}

/// An engine configured for simulated worlds.
pub fn engine(mode: StackMode) -> Result<Engine> {
    if mode == StackMode::Continuations {
        bail!(
            "hegel-sim-wasm: StackMode::Continuations needs the `continuations` feature (the indexable-inc/wasmtime fork, branch sim-stacks), which this build lacks"
        );
    }
    let mut c = Config::new();
    c.cranelift_opt_level(wasmtime::OptLevel::Speed);
    Engine::new(&c)
}

const ASYNCIFY_BUF: u32 = 256 << 10;
const WASI_ENOSYS: i32 = 52;

pub(crate) fn wasi_errno(e: Errno) -> i32 {
    match e {
        Errno::EPERM => 63,
        Errno::ENOENT => 44,
        Errno::EINTR => 27,
        Errno::EIO => 29,
        Errno::EBADF => 8,
        Errno::EAGAIN => 6,
        Errno::ENOMEM => 48,
        Errno::EBUSY => 10,
        Errno::EEXIST => 20,
        Errno::EXDEV => 75,
        Errno::ENOTDIR => 54,
        Errno::EISDIR => 31,
        Errno::EINVAL => 28,
        Errno::EFBIG => 22,
        Errno::ENOSPC => 51,
        Errno::ESPIPE => 70,
        Errno::EROFS => 69,
        Errno::ENOSYS => 52,
        Errno::ENOTEMPTY => 55,
        Errno::EDEADLK => 16,
        _ => 29,
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Entry {
    Start,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Coro {
    pub(crate) pid: u32,
    pub(crate) entry: Entry,
    pub(crate) buf: u32,
    pub(crate) started: bool,
    /// The encoded `SysResult` (or a plain value) the thread resumes with.
    pub(crate) reply: Option<Reply>,
    /// An encoded syscall to retry before resuming (it blocked).
    pub(crate) retry: Option<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) enum Reply {
    Sys(Vec<u8>),
    Value(i64),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct OpenFd {
    pub(crate) kernel: i32,
    pub(crate) path: String,
    pub(crate) dir: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ProcTable {
    pub(crate) name: String,
    pub(crate) program: usize,
    pub(crate) args: Vec<String>,
    pub(crate) fds: BTreeMap<u32, OpenFd>,
    pub(crate) next_fd: u32,
    pub(crate) main: u32,
}

/// The host's own state: small and serializable.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct HostTables {
    pub(crate) procs: BTreeMap<u32, ProcTable>,
    pub(crate) coros: BTreeMap<u32, Coro>,
    pub(crate) next_pid: u32,
    pub(crate) crashes: u32,
    pub(crate) stdout: Vec<u8>,
    pub(crate) notes: Vec<String>,
    pub(crate) exits: Vec<(u32, String, i32)>,
}

#[derive(Clone)]
pub(crate) struct Asy {
    start_unwind: TypedFunc<u32, ()>,
    stop_unwind: TypedFunc<(), ()>,
    start_rewind: TypedFunc<u32, ()>,
    stop_rewind: TypedFunc<(), ()>,
    get_state: TypedFunc<(), u32>,
    start: TypedFunc<(), ()>,
}

#[derive(Clone)]
pub(crate) struct Live {
    pub(crate) instance: Instance,
    pub(crate) memory: Memory,
    asy: Asy,
}

/// Why wasm execution stopped early: carried through a trap.
#[derive(Debug)]
pub(crate) enum Stop {
    Exit(i32),
    Crash(Crash),
}

impl std::fmt::Display for Stop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for Stop {}

pub(crate) struct Host {
    choices: *const (dyn Choices + 'static),
    pub(crate) machine: Option<Machine>,
    pub(crate) live: BTreeMap<u32, Live>,
    pub(crate) t: HostTables,
    current: Option<u32>,
    next: Option<u32>,
}

impl Host {
    fn choices(&self) -> &dyn Choices {
        // SAFETY: `World<'c>` keeps the `&'c dyn Choices` alive while its store
        // (and this host) exists.
        unsafe { &*self.choices }
    }

    fn machine(&self) -> Machine {
        self.machine
            .clone()
            .unwrap_or_else(|| unreachable!("hegel-sim-wasm: no machine instance"))
    }
}

/// How a run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every process exited: (pid, name, code).
    Exited(Vec<(u32, String, i32)>),
    /// A drawn crash fired.
    Crashed(String),
}

/// A simulated world whose every choice comes from `'c` choices.
pub struct World<'c> {
    pub(crate) engine: Engine,
    pub(crate) store: Store<Host>,
    pub(crate) linker: Linker<Host>,
    pub(crate) machine_module: MachineModule,
    pub(crate) programs: Vec<Program>,
    pub(crate) cfg: WasmConfig,
    pub(crate) _choices: std::marker::PhantomData<&'c dyn Choices>,
}

fn rd_u32(m: &[u8], at: u32) -> u32 {
    let a = at as usize;
    u32::from_le_bytes([m[a], m[a + 1], m[a + 2], m[a + 3]])
}

fn wr_u32(m: &mut [u8], at: u32, v: u32) {
    m[at as usize..at as usize + 4].copy_from_slice(&v.to_le_bytes());
}

fn wr_u64(m: &mut [u8], at: u32, v: u64) {
    m[at as usize..at as usize + 8].copy_from_slice(&v.to_le_bytes());
}

fn rd_str(m: &[u8], ptr: u32, len: u32) -> Result<String> {
    String::from_utf8(m[ptr as usize..(ptr + len) as usize].to_vec())
        .map_err(|_| anyhow!("hegel-sim-wasm: a non-UTF-8 path"))
}

fn join(dir: &str, rel: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for p in dir.split('/').chain(rel.split('/')) {
        match p {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    format!("/{}", parts.join("/"))
}

fn cur(caller: &Caller<'_, Host>) -> (u32, u32, Live) {
    let tid = caller
        .data()
        .current
        .unwrap_or_else(|| unreachable!("hegel-sim-wasm: an import ran with no current thread"));
    let pid = caller.data().t.coros[&tid].pid;
    (tid, pid, caller.data().live[&pid].clone())
}

/// At an import that is a switch point: if this thread is rewinding, stop the
/// rewind and return what it resumes with.
fn resumed(caller: &mut Caller<'_, Host>) -> Result<Option<Reply>> {
    let (tid, _, live) = cur(caller);
    if live.asy.get_state.call(&mut *caller, ())? == 2 {
        live.asy.stop_rewind.call(&mut *caller, ())?;
        return Ok(caller
            .data_mut()
            .t
            .coros
            .get_mut(&tid)
            .and_then(|c| c.reply.take()));
    }
    Ok(None)
}

/// Unwind the current thread so the driver can run `next`.
fn suspend(caller: &mut Caller<'_, Host>, next: Option<u32>) -> Result<()> {
    let (tid, _, live) = cur(caller);
    let buf = caller.data().t.coros[&tid].buf;
    let m = live.memory.data_mut(&mut *caller);
    wr_u32(m, buf, buf + 8);
    wr_u32(m, buf + 4, buf + ASYNCIFY_BUF);
    caller.data_mut().next = next;
    live.asy.start_unwind.call(&mut *caller, buf)
}

/// Kill another process now (its instance goes; nothing of it runs again).
fn kill_other(caller: &mut Caller<'_, Host>, pid: u32) -> Result<()> {
    let m = caller.data().machine();
    m.crash(&mut *caller, Crash::Kill(pid))?;
    end_threads(&mut *caller, pid)?;
    let h = caller.data_mut();
    h.live.remove(&pid);
    if let Some(p) = h.t.procs.remove(&pid) {
        h.t.exits.push((pid, p.name, 137));
    }
    Ok(())
}

fn end_threads(mut cx: impl wasmtime::AsContextMut<Data = Host>, pid: u32) -> Result<()> {
    let tids: Vec<u32> = cx
        .as_context()
        .data()
        .t
        .coros
        .iter()
        .filter(|(_, c)| c.pid == pid)
        .map(|(t, _)| *t)
        .collect();
    let m = cx.as_context().data().machine();
    for t in tids {
        m.thread_exit(&mut cx, t)?;
        cx.as_context_mut().data_mut().t.coros.remove(&t);
    }
    Ok(())
}

/// A syscall at a switch point. `finish` writes the result into guest memory
/// and returns the WASI errno.
fn sys(
    caller: &mut Caller<'_, Host>,
    build: impl FnOnce(&mut Caller<'_, Host>) -> Result<std::result::Result<Syscall, i32>>,
    finish: impl FnOnce(&mut Caller<'_, Host>, SysResult) -> Result<i32>,
) -> Result<i32> {
    if let Some(r) = resumed(caller)? {
        let Reply::Sys(bytes) = r else {
            bail!("hegel-sim-wasm: a syscall resumed with a non-syscall reply")
        };
        let r =
            hegel_sim_core::kernel::wire::decode_result(&bytes).map_err(|e| anyhow!("{e:?}"))?;
        return finish(caller, r);
    }
    let call = match build(caller)? {
        Ok(c) => c,
        Err(errno) => return Ok(errno),
    };
    let m = caller.data().machine();
    let (tid, pid, _) = cur(caller);
    match m.maybe_crash(&mut *caller, true)? {
        Crash::None => {}
        Crash::Kill(victim) if victim != pid => kill_other(caller, victim)?,
        c => return Err(Stop::Crash(c).into()),
    }
    let r = m.syscall(&mut *caller, pid, tid, &call)?;
    if let SysResult::Block(_) = r {
        let mut bytes = Vec::new();
        hegel_sim_core::kernel::wire::encode_syscall(&call, &mut bytes);
        if let Some(c) = caller.data_mut().t.coros.get_mut(&tid) {
            c.retry = Some(bytes);
        }
        suspend(caller, None)?;
        return Ok(0);
    }
    let next = m.pick(&mut *caller)?;
    if next == Some(tid) {
        return finish(caller, r);
    }
    let mut bytes = Vec::new();
    hegel_sim_core::kernel::wire::encode_result(&r, &mut bytes);
    if let Some(c) = caller.data_mut().t.coros.get_mut(&tid) {
        c.reply = Some(Reply::Sys(bytes));
    }
    suspend(caller, next)?;
    Ok(0)
}

fn errno_of(r: &SysResult) -> Option<i32> {
    match r {
        SysResult::Err(e) => Some(wasi_errno(*e)),
        _ => None,
    }
}

fn mem(caller: &mut Caller<'_, Host>) -> Memory {
    cur(caller).2.memory
}

fn kfd(caller: &Caller<'_, Host>, fd: u32) -> std::result::Result<OpenFd, i32> {
    let (_, pid, _) = cur(caller);
    caller.data().t.procs[&pid].fds.get(&fd).cloned().ok_or(8)
}

fn gather(m: &[u8], iovs: u32, n: u32) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..n {
        let p = rd_u32(m, iovs + i * 8);
        let l = rd_u32(m, iovs + i * 8 + 4);
        out.extend_from_slice(&m[p as usize..(p + l) as usize]);
    }
    out
}

fn iov_total(m: &[u8], iovs: u32, n: u32) -> usize {
    (0..n)
        .map(|i| rd_u32(m, iovs + i * 8 + 4) as usize)
        .sum::<usize>()
        .min(512 << 10)
}

fn scatter(m: &mut [u8], iovs: u32, n: u32, data: &[u8]) {
    let mut off = 0;
    for i in 0..n {
        let p = rd_u32(m, iovs + i * 8) as usize;
        let l = rd_u32(m, iovs + i * 8 + 4) as usize;
        let k = l.min(data.len() - off);
        m[p..p + k].copy_from_slice(&data[off..off + k]);
        off += k;
        if off == data.len() {
            break;
        }
    }
}

fn write_filestat(m: &mut [u8], at: u32, ino: u64, size: u64, dir: bool) {
    m[at as usize..at as usize + 64].fill(0);
    wr_u64(m, at + 8, ino);
    m[at as usize + 16] = if dir { 3 } else { 4 };
    wr_u64(m, at + 24, 1);
    wr_u64(m, at + 32, size);
}

fn define_wasi(linker: &mut Linker<Host>) -> Result<()> {
    const W: &str = "wasi_snapshot_preview1";
    linker.func_wrap(
        W,
        "args_sizes_get",
        |mut c: Caller<'_, Host>, argc: u32, size: u32| -> Result<i32> {
            let (_, pid, live) = cur(&c);
            let args = c.data().t.procs[&pid].args.clone();
            let m = live.memory.data_mut(&mut c);
            wr_u32(m, argc, args.len() as u32);
            wr_u32(m, size, args.iter().map(|a| a.len() as u32 + 1).sum());
            Ok(0)
        },
    )?;
    linker.func_wrap(
        W,
        "args_get",
        |mut c: Caller<'_, Host>, argv: u32, buf: u32| -> Result<i32> {
            let (_, pid, live) = cur(&c);
            let args = c.data().t.procs[&pid].args.clone();
            let m = live.memory.data_mut(&mut c);
            let mut at = buf;
            for (i, a) in args.iter().enumerate() {
                wr_u32(m, argv + i as u32 * 4, at);
                m[at as usize..at as usize + a.len()].copy_from_slice(a.as_bytes());
                m[at as usize + a.len()] = 0;
                at += a.len() as u32 + 1;
            }
            Ok(0)
        },
    )?;
    linker.func_wrap(
        W,
        "environ_sizes_get",
        |mut c: Caller<'_, Host>, n: u32, size: u32| -> Result<i32> {
            let m = mem(&mut c).data_mut(&mut c);
            wr_u32(m, n, 0);
            wr_u32(m, size, 0);
            Ok(0)
        },
    )?;
    linker.func_wrap(
        W,
        "environ_get",
        |_: Caller<'_, Host>, _: u32, _: u32| -> Result<i32> { Ok(0) },
    )?;
    linker.func_wrap(
        W,
        "fd_prestat_get",
        |mut c: Caller<'_, Host>, fd: u32, out: u32| -> Result<i32> {
            if fd != 3 {
                return Ok(8);
            }
            let m = mem(&mut c).data_mut(&mut c);
            wr_u32(m, out, 0);
            wr_u32(m, out + 4, 1);
            Ok(0)
        },
    )?;
    linker.func_wrap(
        W,
        "fd_prestat_dir_name",
        |mut c: Caller<'_, Host>, fd: u32, p: u32, _len: u32| -> Result<i32> {
            if fd != 3 {
                return Ok(8);
            }
            mem(&mut c).data_mut(&mut c)[p as usize] = b'/';
            Ok(0)
        },
    )?;
    linker.func_wrap(
        W,
        "fd_fdstat_get",
        |mut c: Caller<'_, Host>, fd: u32, out: u32| -> Result<i32> {
            let kind = match fd {
                0..=2 => 2u8,
                3 => 3,
                _ => match kfd(&c, fd) {
                    Ok(f) if f.dir => 3,
                    Ok(_) => 4,
                    Err(e) => return Ok(e),
                },
            };
            let m = mem(&mut c).data_mut(&mut c);
            m[out as usize..out as usize + 24].fill(0);
            m[out as usize] = kind;
            wr_u64(m, out + 8, u64::MAX);
            wr_u64(m, out + 16, u64::MAX);
            Ok(0)
        },
    )?;
    linker.func_wrap(
        W,
        "fd_fdstat_set_flags",
        |_: Caller<'_, Host>, _: u32, _: u32| -> Result<i32> { Ok(WASI_ENOSYS) },
    )?;
    linker.func_wrap(
        W,
        "clock_time_get",
        |mut c: Caller<'_, Host>, id: u32, _prec: u64, out: u64| -> Result<i32> {
            let m = c.data().machine();
            let now = m.now_ns(&mut c)?;
            let t = if id == 0 {
                now + 1_767_225_600_000_000_000
            } else {
                now
            };
            wr_u64(mem(&mut c).data_mut(&mut c), out as u32, t);
            Ok(0)
        },
    )?;
    linker.func_wrap(
        W,
        "random_get",
        |mut c: Caller<'_, Host>, buf: u32, len: u32| -> Result<i32> {
            let bytes: Vec<u8> = (0..len)
                .map(|_| c.data().choices().int("wasi.random_byte", 0, 255) as u8)
                .collect();
            mem(&mut c).write(&mut c, buf as usize, &bytes)?;
            Ok(0)
        },
    )?;
    linker.func_wrap(
        W,
        "proc_exit",
        |_: Caller<'_, Host>, code: i32| -> Result<()> { Err(Stop::Exit(code).into()) },
    )?;
    linker.func_wrap(W, "sched_yield", |mut c: Caller<'_, Host>| -> Result<i32> {
        if resumed(&mut c)?.is_some() {
            return Ok(0);
        }
        let (tid, _, _) = cur(&c);
        let m = c.data().machine();
        m.yielded(&mut c, tid)?;
        let next = m.pick(&mut c)?;
        if next != Some(tid) {
            if let Some(co) = c.data_mut().t.coros.get_mut(&tid) {
                co.reply = Some(Reply::Value(0));
            }
            suspend(&mut c, next)?;
        }
        Ok(0)
    })?;
    linker.func_wrap(W, "poll_oneoff", |mut c: Caller<'_, Host>, sub: u32, ev: u32, n: u32, nev: u32| -> Result<i32> {
        if n != 1 {
            bail!("hegel-sim-wasm: poll_oneoff with {n} subscriptions is not modelled (one clock subscription is)");
        }
        let memory = mem(&mut c);
        if let Some(_r) = resumed(&mut c)? {
            let m = memory.data_mut(&mut c);
            let ud = u64::from_le_bytes(m[sub as usize..sub as usize + 8].try_into().unwrap_or_default());
            m[ev as usize..ev as usize + 32].fill(0);
            wr_u64(m, ev, ud);
            wr_u32(m, nev, 1);
            return Ok(0);
        }
        let (tag, timeout, flags) = {
            let m = memory.data(&c);
            let s = sub as usize;
            (m[s + 8], u64::from_le_bytes(m[s + 24..s + 32].try_into().unwrap_or_default()), u16::from_le_bytes([m[s + 40], m[s + 41]]))
        };
        if tag != 0 {
            bail!("hegel-sim-wasm: poll_oneoff on an fd is not modelled (a clock subscription is)");
        }
        let (tid, _, _) = cur(&c);
        let m = c.data().machine();
        let now = m.now_ns(&mut c)?;
        let deadline = if flags & 1 != 0 { timeout } else { now.saturating_add(timeout) };
        m.sleep_until(&mut c, tid, deadline)?;
        if let Some(co) = c.data_mut().t.coros.get_mut(&tid) {
            co.reply = Some(Reply::Value(0));
        }
        suspend(&mut c, None)?;
        Ok(0)
    })?;
    linker.func_wrap(
        W,
        "fd_write",
        |mut c: Caller<'_, Host>, fd: u32, iovs: u32, n: u32, out: u32| -> Result<i32> {
            if fd == 1 || fd == 2 {
                let memory = mem(&mut c);
                let data = gather(memory.data(&c), iovs, n);
                c.data_mut().t.stdout.extend_from_slice(&data);
                wr_u32(memory.data_mut(&mut c), out, data.len() as u32);
                return Ok(0);
            }
            sys(
                &mut c,
                |c| {
                    let f = match kfd(c, fd) {
                        Ok(f) => f,
                        Err(e) => return Ok(Err(e)),
                    };
                    let data = gather(mem(c).data(&*c), iovs, n);
                    Ok(Ok(Syscall::Write { fd: f.kernel, data }))
                },
                |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Count(k)) = r else {
                        bail!("write returned {r:?}")
                    };
                    wr_u32(mem(c).data_mut(&mut *c), out, k as u32);
                    Ok(0)
                },
            )
        },
    )?;
    linker.func_wrap(
        W,
        "fd_pwrite",
        |mut c: Caller<'_, Host>, fd: u32, iovs: u32, n: u32, off: u64, out: u32| -> Result<i32> {
            sys(
                &mut c,
                |c| {
                    let f = match kfd(c, fd) {
                        Ok(f) => f,
                        Err(e) => return Ok(Err(e)),
                    };
                    let data = gather(mem(c).data(&*c), iovs, n);
                    Ok(Ok(Syscall::Pwrite {
                        fd: f.kernel,
                        data,
                        offset: off,
                    }))
                },
                |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Count(k)) = r else {
                        bail!("pwrite returned {r:?}")
                    };
                    wr_u32(mem(c).data_mut(&mut *c), out, k as u32);
                    Ok(0)
                },
            )
        },
    )?;
    linker.func_wrap(
        W,
        "fd_read",
        |mut c: Caller<'_, Host>, fd: u32, iovs: u32, n: u32, out: u32| -> Result<i32> {
            if fd == 0 {
                wr_u32(mem(&mut c).data_mut(&mut c), out, 0);
                return Ok(0);
            }
            sys(
                &mut c,
                |c| {
                    let f = match kfd(c, fd) {
                        Ok(f) => f,
                        Err(e) => return Ok(Err(e)),
                    };
                    let len = iov_total(mem(c).data(&*c), iovs, n);
                    Ok(Ok(Syscall::Read { fd: f.kernel, len }))
                },
                |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Bytes(b)) = r else {
                        bail!("read returned {r:?}")
                    };
                    let m = mem(c).data_mut(&mut *c);
                    scatter(m, iovs, n, &b);
                    wr_u32(m, out, b.len() as u32);
                    Ok(0)
                },
            )
        },
    )?;
    linker.func_wrap(
        W,
        "fd_pread",
        |mut c: Caller<'_, Host>, fd: u32, iovs: u32, n: u32, off: u64, out: u32| -> Result<i32> {
            sys(
                &mut c,
                |c| {
                    let f = match kfd(c, fd) {
                        Ok(f) => f,
                        Err(e) => return Ok(Err(e)),
                    };
                    let len = iov_total(mem(c).data(&*c), iovs, n);
                    Ok(Ok(Syscall::Pread {
                        fd: f.kernel,
                        len,
                        offset: off,
                    }))
                },
                |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Bytes(b)) = r else {
                        bail!("pread returned {r:?}")
                    };
                    let m = mem(c).data_mut(&mut *c);
                    scatter(m, iovs, n, &b);
                    wr_u32(m, out, b.len() as u32);
                    Ok(0)
                },
            )
        },
    )?;
    linker.func_wrap(
        W,
        "fd_seek",
        |mut c: Caller<'_, Host>, fd: u32, off: i64, whence: u32, out: u32| -> Result<i32> {
            sys(
                &mut c,
                |c| {
                    let f = match kfd(c, fd) {
                        Ok(f) => f,
                        Err(e) => return Ok(Err(e)),
                    };
                    let whence = match whence {
                        0 => Whence::Set(off as u64),
                        1 => Whence::Cur(off),
                        2 => Whence::End(off),
                        _ => return Ok(Err(28)),
                    };
                    Ok(Ok(Syscall::Seek {
                        fd: f.kernel,
                        whence,
                    }))
                },
                |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Offset(o)) = r else {
                        bail!("lseek returned {r:?}")
                    };
                    wr_u64(mem(c).data_mut(&mut *c), out, o);
                    Ok(0)
                },
            )
        },
    )?;
    linker.func_wrap(
        W,
        "fd_tell",
        |mut c: Caller<'_, Host>, fd: u32, out: u32| -> Result<i32> {
            sys(
                &mut c,
                |c| {
                    Ok(kfd(c, fd).map(|f| Syscall::Seek {
                        fd: f.kernel,
                        whence: Whence::Cur(0),
                    }))
                },
                |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Offset(o)) = r else {
                        bail!("lseek returned {r:?}")
                    };
                    wr_u64(mem(c).data_mut(&mut *c), out, o);
                    Ok(0)
                },
            )
        },
    )?;
    for (name, data) in [("fd_sync", false), ("fd_datasync", true)] {
        linker.func_wrap(
            W,
            name,
            move |mut c: Caller<'_, Host>, fd: u32| -> Result<i32> {
                sys(
                    &mut c,
                    |c| {
                        if fd == 3 {
                            return Ok(Err(0));
                        }
                        Ok(kfd(c, fd).map(|f| {
                            if data {
                                Syscall::Fdatasync { fd: f.kernel }
                            } else {
                                Syscall::Fsync { fd: f.kernel }
                            }
                        }))
                    },
                    |_, r| Ok(errno_of(&r).unwrap_or(0)),
                )
            },
        )?;
    }
    linker.func_wrap(
        W,
        "fd_close",
        |mut c: Caller<'_, Host>, fd: u32| -> Result<i32> {
            if fd <= 3 {
                return Ok(0);
            }
            sys(
                &mut c,
                |c| {
                    let f = kfd(c, fd);
                    let (_, pid, _) = cur(c);
                    c.data_mut()
                        .t
                        .procs
                        .get_mut(&pid)
                        .map(|p| p.fds.remove(&fd));
                    Ok(f.map(|f| Syscall::Close { fd: f.kernel }))
                },
                |_, r| Ok(errno_of(&r).unwrap_or(0)),
            )
        },
    )?;
    linker.func_wrap(
        W,
        "fd_filestat_get",
        |mut c: Caller<'_, Host>, fd: u32, out: u32| -> Result<i32> {
            sys(
                &mut c,
                |c| Ok(kfd(c, fd).map(|f| Syscall::Fstat { fd: f.kernel })),
                |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Stat(s)) = r else {
                        bail!("fstat returned {r:?}")
                    };
                    write_filestat(mem(c).data_mut(&mut *c), out, s.ino, s.size, s.is_dir);
                    Ok(0)
                },
            )
        },
    )?;
    linker.func_wrap(
        W,
        "fd_filestat_set_size",
        |mut c: Caller<'_, Host>, fd: u32, size: u64| -> Result<i32> {
            sys(
                &mut c,
                |c| {
                    Ok(kfd(c, fd).map(|f| Syscall::Ftruncate {
                        fd: f.kernel,
                        len: size,
                    }))
                },
                |_, r| Ok(errno_of(&r).unwrap_or(0)),
            )
        },
    )?;
    fn dir_path(
        c: &Caller<'_, Host>,
        fd: u32,
        p: u32,
        l: u32,
    ) -> Result<std::result::Result<String, i32>> {
        let base = if fd == 3 {
            "/".to_owned()
        } else {
            match kfd(c, fd) {
                Ok(f) => f.path,
                Err(e) => return Ok(Err(e)),
            }
        };
        let (_, _, live) = cur(c);
        let rel = rd_str(live.memory.data(c), p, l)?;
        Ok(Ok(join(&base, &rel)))
    }
    linker.func_wrap(
        W,
        "path_open",
        |mut c: Caller<'_, Host>,
         dirfd: u32,
         _df: u32,
         p: u32,
         l: u32,
         oflags: u32,
         rights: u64,
         _ri: u64,
         fdflags: u32,
         out: u32|
         -> Result<i32> {
            let path = match dir_path(&c, dirfd, p, l)? {
                Ok(p) => p,
                Err(e) => return Ok(e),
            };
            let directory = oflags & 2 != 0;
            let flags = OpenFlags {
                read: rights & (1 << 1) != 0 || directory,
                write: rights & (1 << 6) != 0,
                create: oflags & 1 != 0,
                excl: oflags & 4 != 0,
                trunc: oflags & 8 != 0,
                append: fdflags & 1 != 0,
                dsync: fdflags & 2 != 0,
                sync: fdflags & 16 != 0,
                directory,
            };
            let p2 = path.clone();
            sys(
                &mut c,
                move |_| Ok(Ok(Syscall::Open { path: p2, flags })),
                move |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Fd(k)) = r else {
                        bail!("open returned {r:?}")
                    };
                    let (_, pid, live) = cur(c);
                    let p = c
                        .data_mut()
                        .t
                        .procs
                        .get_mut(&pid)
                        .unwrap_or_else(|| unreachable!());
                    let fd = p.next_fd;
                    p.next_fd += 1;
                    p.fds.insert(
                        fd,
                        OpenFd {
                            kernel: k,
                            path,
                            dir: directory,
                        },
                    );
                    wr_u32(live.memory.data_mut(&mut *c), out, fd);
                    Ok(0)
                },
            )
        },
    )?;
    linker.func_wrap(
        W,
        "path_filestat_get",
        |mut c: Caller<'_, Host>, fd: u32, _fl: u32, p: u32, l: u32, out: u32| -> Result<i32> {
            let path = match dir_path(&c, fd, p, l)? {
                Ok(p) => p,
                Err(e) => return Ok(e),
            };
            sys(
                &mut c,
                move |_| Ok(Ok(Syscall::Stat { path })),
                |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Stat(s)) = r else {
                        bail!("stat returned {r:?}")
                    };
                    write_filestat(mem(c).data_mut(&mut *c), out, s.ino, s.size, s.is_dir);
                    Ok(0)
                },
            )
        },
    )?;
    linker.func_wrap(
        W,
        "path_rename",
        |mut c: Caller<'_, Host>,
         fd: u32,
         p: u32,
         l: u32,
         fd2: u32,
         p2: u32,
         l2: u32|
         -> Result<i32> {
            let (from, to) = match (dir_path(&c, fd, p, l)?, dir_path(&c, fd2, p2, l2)?) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => return Ok(e),
            };
            sys(
                &mut c,
                move |_| Ok(Ok(Syscall::Rename { from, to })),
                |_, r| Ok(errno_of(&r).unwrap_or(0)),
            )
        },
    )?;
    for name in [
        "path_unlink_file",
        "path_create_directory",
        "path_remove_directory",
    ] {
        linker.func_wrap(
            W,
            name,
            move |mut c: Caller<'_, Host>, fd: u32, p: u32, l: u32| -> Result<i32> {
                let path = match dir_path(&c, fd, p, l)? {
                    Ok(p) => p,
                    Err(e) => return Ok(e),
                };
                let call = match name {
                    "path_unlink_file" => Syscall::Unlink { path },
                    "path_create_directory" => Syscall::Mkdir { path },
                    _ => Syscall::Rmdir { path },
                };
                sys(
                    &mut c,
                    move |_| Ok(Ok(call)),
                    |_, r| Ok(errno_of(&r).unwrap_or(0)),
                )
            },
        )?;
    }
    linker.func_wrap(
        W,
        "fd_readdir",
        |mut c: Caller<'_, Host>,
         fd: u32,
         buf: u32,
         len: u32,
         cookie: u64,
         used: u32|
         -> Result<i32> {
            sys(
                &mut c,
                |c| Ok(kfd(c, fd).map(|f| Syscall::ReadDir { path: f.path })),
                move |c, r| {
                    if let Some(e) = errno_of(&r) {
                        return Ok(e);
                    }
                    let SysResult::Ok(SysOk::Entries(es)) = r else {
                        bail!("readdir returned {r:?}")
                    };
                    let mut out = Vec::new();
                    for (i, e) in es.iter().enumerate().skip(cookie as usize) {
                        out.extend_from_slice(&(i as u64 + 1).to_le_bytes());
                        out.extend_from_slice(&e.ino.to_le_bytes());
                        out.extend_from_slice(&(e.name.len() as u32).to_le_bytes());
                        out.push(if e.is_dir { 3 } else { 4 });
                        out.extend_from_slice(&[0, 0, 0]);
                        out.extend_from_slice(e.name.as_bytes());
                    }
                    out.truncate(len as usize);
                    let m = mem(c).data_mut(&mut *c);
                    m[buf as usize..buf as usize + out.len()].copy_from_slice(&out);
                    wr_u32(m, used, out.len() as u32);
                    Ok(0)
                },
            )
        },
    )?;
    linker.func_wrap("hegel", "yield", |mut c: Caller<'_, Host>| -> Result<()> {
        if resumed(&mut c)?.is_some() {
            return Ok(());
        }
        let (tid, _, _) = cur(&c);
        let m = c.data().machine();
        let next = m.pick(&mut c)?;
        if next != Some(tid) {
            if let Some(co) = c.data_mut().t.coros.get_mut(&tid) {
                co.reply = Some(Reply::Value(0));
            }
            suspend(&mut c, next)?;
        }
        Ok(())
    })?;
    linker.func_wrap("hegel", "wait32", |_: Caller<'_, Host>, _a: u32, _e: i32, _t: i64, _o: u32| -> Result<i32> {
        bail!("hegel-sim-wasm: memory.atomic.wait32 needs threads, which phase 2a does not model yet")
    })?;
    linker.func_wrap("hegel", "wait64", |_: Caller<'_, Host>, _a: u32, _e: i64, _t: i64, _o: u32| -> Result<i32> {
        bail!("hegel-sim-wasm: memory.atomic.wait64 needs threads, which phase 2a does not model yet")
    })?;
    linker.func_wrap(
        "hegel",
        "notify",
        |_: Caller<'_, Host>, _a: u32, _n: u32, _o: u32| -> Result<i32> { Ok(0) },
    )?;
    Ok(())
}

pub(crate) fn make_linker(engine: &Engine) -> Result<Linker<Host>> {
    let mut linker = Linker::new(engine);
    define_wasi(&mut linker)?;
    linker.func_wrap(
        "hegel",
        "int",
        |c: Caller<'_, Host>, lo: i64, hi: i64| -> i64 {
            c.data().choices().int("machine", lo as u64, hi as u64) as i64
        },
    )?;
    linker.func_wrap("hegel", "coin", |c: Caller<'_, Host>, p: f64| -> i32 {
        i32::from(c.data().choices().coin("machine", p))
    })?;
    Ok(linker)
}

/// Define every import of `module` the linker lacks as a function that returns
/// WASI's `ENOSYS` and notes the call (a real errno; the name is in the notes).
pub(crate) fn define_missing(
    linker: &mut Linker<Host>,
    store: &mut Store<Host>,
    module: &Module,
) -> Result<()> {
    for imp in module.imports() {
        if linker.get(&mut *store, imp.module(), imp.name()).is_some() {
            continue;
        }
        let wasmtime::ExternType::Func(ft) = imp.ty() else {
            bail!(
                "hegel-sim-wasm: import {}.{} is not a function and has no model",
                imp.module(),
                imp.name()
            );
        };
        let label = format!("{}.{}", imp.module(), imp.name());
        let f = Func::new(&mut *store, ft.clone(), move |mut c, _args, results| {
            c.data_mut()
                .t
                .notes
                .push(format!("unmodelled import {label} returned ENOSYS"));
            for r in results.iter_mut() {
                *r = Val::I32(WASI_ENOSYS);
            }
            Ok(())
        });
        linker.define(&mut *store, imp.module(), imp.name(), Extern::Func(f))?;
    }
    Ok(())
}

pub(crate) fn instantiate_live(
    linker: &mut Linker<Host>,
    store: &mut Store<Host>,
    module: &Module,
) -> Result<Live> {
    define_missing(linker, store, module)?;
    let instance = linker.instantiate(&mut *store, module)?;
    let memory = instance
        .get_memory(&mut *store, "memory")
        .ok_or_else(|| anyhow!("hegel-sim-wasm: the program exports no memory"))?;
    macro_rules! f {
        ($n:literal) => {
            instance.get_typed_func(&mut *store, $n).with_context(|| {
                format!(
                    "hegel-sim-wasm: the program lacks `{}` (was it prepared?)",
                    $n
                )
            })?
        };
    }
    let asy = Asy {
        start_unwind: f!("asyncify_start_unwind"),
        stop_unwind: f!("asyncify_stop_unwind"),
        start_rewind: f!("asyncify_start_rewind"),
        stop_rewind: f!("asyncify_stop_rewind"),
        get_state: f!("asyncify_get_state"),
        start: f!("_start"),
    };
    Ok(Live {
        instance,
        memory,
        asy,
    })
}

impl<'c> World<'c> {
    /// A fresh machine (draws its swarm and scheduler) with no processes.
    pub fn new(
        engine: &Engine,
        machine: &MachineModule,
        programs: Vec<Program>,
        cfg: WasmConfig,
        choices: &'c dyn Choices,
    ) -> Result<World<'c>> {
        let mut w = World::empty(engine, machine, programs, cfg, choices)?;
        let instance = w.linker.instantiate(&mut w.store, &machine.module)?;
        let m = Machine::new(&mut w.store, instance)?;
        let cfg_bytes = w.cfg.encode();
        m.init(&mut w.store, &cfg_bytes)?;
        w.store.data_mut().machine = Some(m);
        w.store.data_mut().t.next_pid = 1;
        Ok(w)
    }

    pub(crate) fn empty(
        engine: &Engine,
        machine: &MachineModule,
        programs: Vec<Program>,
        cfg: WasmConfig,
        choices: &'c dyn Choices,
    ) -> Result<World<'c>> {
        // SAFETY: erases the choices' lifetime; `World<'c>` (and so its store)
        // cannot outlive `'c`.
        let choices: *const (dyn Choices + 'static) = unsafe {
            std::mem::transmute::<*const dyn Choices, *const (dyn Choices + 'static)>(choices)
        };
        let store = Store::new(
            engine,
            Host {
                choices,
                machine: None,
                live: BTreeMap::new(),
                t: HostTables::default(),
                current: None,
                next: None,
            },
        );
        Ok(World {
            engine: engine.clone(),
            linker: make_linker(engine)?,
            store,
            machine_module: machine.clone(),
            programs,
            cfg,
            _choices: std::marker::PhantomData,
        })
    }

    fn machine(&self) -> Machine {
        self.store.data().machine()
    }

    /// Redraw the swarm, scheduler and crash budget from `cfg` (between runs,
    /// with no live thread), keeping disk, clock and processes. A world
    /// restored from a snapshot calls this to draw its own chaos.
    pub fn reconfigure(&mut self, cfg: WasmConfig) -> Result<()> {
        let m = self.machine();
        m.reconfigure(&mut self.store, &cfg.encode())?;
        self.cfg = cfg;
        Ok(())
    }

    /// Start a process running program `program` (an index into the programs
    /// the world was made with) with `args` (argv[0] is its name).
    pub fn spawn(&mut self, program: usize, args: &[&str]) -> Result<u32> {
        let p = self
            .programs
            .get(program)
            .ok_or_else(|| anyhow!("hegel-sim-wasm: no program #{program}"))?
            .clone();
        let live = instantiate_live(&mut self.linker, &mut self.store, &p.module)?;
        let pages = live
            .memory
            .grow(&mut self.store, (ASYNCIFY_BUF as u64).div_ceil(65536))?;
        let buf = (pages * 65536) as u32;
        let m = self.machine();
        let pid = self.store.data().t.next_pid;
        self.store.data_mut().t.next_pid += 1;
        m.proc_spawn(&mut self.store, pid)?;
        let tid = m.thread_spawn(&mut self.store, pid)?;
        let mut argv = vec![p.name.clone()];
        argv.extend(args.iter().map(|s| (*s).to_owned()));
        let h = self.store.data_mut();
        h.live.insert(pid, live);
        h.t.procs.insert(
            pid,
            ProcTable {
                name: p.name.clone(),
                program,
                args: argv,
                fds: BTreeMap::new(),
                next_fd: 4,
                main: tid,
            },
        );
        h.t.coros.insert(
            tid,
            Coro {
                pid,
                entry: Entry::Start,
                buf,
                started: false,
                reply: None,
                retry: None,
            },
        );
        Ok(pid)
    }

    fn end_process(&mut self, pid: u32, code: i32) -> Result<()> {
        let m = self.machine();
        end_threads(&mut self.store, pid)?;
        m.proc_exit(&mut self.store, pid)?;
        let h = self.store.data_mut();
        h.live.remove(&pid);
        if let Some(p) = h.t.procs.remove(&pid) {
            h.t.exits.push((pid, p.name, code));
        }
        Ok(())
    }

    fn crash_all(&mut self, c: Crash) -> Result<String> {
        let m = self.machine();
        let pids: Vec<u32> = self.store.data().t.procs.keys().copied().collect();
        if let Crash::Kill(p) = c {
            m.crash(&mut self.store, c)?;
            end_threads(&mut self.store, p)?;
            let h = self.store.data_mut();
            h.live.remove(&p);
            h.t.procs.remove(&p);
            h.t.crashes += 1;
            return Ok(format!("kill -9 of process {p}"));
        }
        for p in pids {
            end_threads(&mut self.store, p)?;
            let h = self.store.data_mut();
            h.live.remove(&p);
            h.t.procs.remove(&p);
        }
        m.crash(&mut self.store, c)?;
        self.store.data_mut().t.crashes += 1;
        Ok(format!("{c:?}"))
    }

    /// Run until every process exits or a drawn crash fires.
    pub fn run(&mut self) -> Result<Outcome> {
        self.run_until(|_| false)
            .map(|o| o.unwrap_or_else(|| unreachable!()))
    }

    /// Run until every process exits, a crash fires, or `stop(step)` holds at a
    /// point where every thread is suspended (`Ok(None)`: a snapshot can be
    /// taken now and `run` continues later).
    pub fn run_until(&mut self, mut stop: impl FnMut(u64) -> bool) -> Result<Option<Outcome>> {
        let m = self.machine();
        m.begin_epoch(&mut self.store)?;
        self.store.data_mut().t.exits.clear();
        loop {
            if stop(m.step(&mut self.store)?) {
                return Ok(None);
            }
            let next = self.store.data_mut().next.take();
            let tid = match next {
                Some(t) => t,
                None => match m.pick(&mut self.store)? {
                    Some(t) => t,
                    None => {
                        return Ok(Some(Outcome::Exited(std::mem::take(
                            &mut self.store.data_mut().t.exits,
                        ))));
                    }
                },
            };
            let Some(coro) = self.store.data().t.coros.get(&tid).cloned() else {
                bail!(
                    "hegel-sim-wasm: the scheduler picked thread {tid}, which the host does not know"
                );
            };
            if let Some(bytes) = coro.retry.clone() {
                let call = hegel_sim_core::kernel::wire::decode_syscall(&bytes)
                    .map_err(|e| anyhow!("{e:?}"))?;
                let r = m.syscall(&mut self.store, coro.pid, tid, &call)?;
                let c = self
                    .store
                    .data_mut()
                    .t
                    .coros
                    .get_mut(&tid)
                    .unwrap_or_else(|| unreachable!());
                if let SysResult::Block(_) = r {
                    continue;
                }
                c.retry = None;
                let mut out = Vec::new();
                hegel_sim_core::kernel::wire::encode_result(&r, &mut out);
                c.reply = Some(Reply::Sys(out));
            }
            let live = self.store.data().live[&coro.pid].clone();
            self.store.data_mut().current = Some(tid);
            if coro.started {
                live.asy.start_rewind.call(&mut self.store, coro.buf)?;
            } else {
                if let Some(c) = self.store.data_mut().t.coros.get_mut(&tid) {
                    c.started = true;
                }
            }
            let r = match coro.entry {
                Entry::Start => live.asy.start.call(&mut self.store, ()),
            };
            self.store.data_mut().current = None;
            match r {
                Ok(()) => {
                    if live.asy.get_state.call(&mut self.store, ())? == 1 {
                        live.asy.stop_unwind.call(&mut self.store, ())?;
                    } else {
                        self.end_process(coro.pid, 0)?;
                    }
                }
                Err(e) => match e.downcast::<Stop>() {
                    Ok(Stop::Exit(code)) => self.end_process(coro.pid, code)?,
                    Ok(Stop::Crash(c)) => return Ok(Some(Outcome::Crashed(self.crash_all(c)?))),
                    Err(e) => {
                        let out = String::from_utf8_lossy(&self.store.data().t.stdout).into_owned();
                        let trace = m.trace(&mut self.store).unwrap_or_default();
                        return Err(e.context(format!("hegel-sim-wasm: thread {tid} of process {} trapped\nstdout/stderr:\n{out}\ntrace:\n{trace}", coro.pid)));
                    }
                },
            }
        }
    }

    /// A file's page-cache contents (after a crash, the resolved state).
    pub fn read_file(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        let m = self.machine();
        m.read_file(&mut self.store, path)
    }

    /// The machine's trace so far.
    pub fn trace(&mut self) -> Result<String> {
        let m = self.machine();
        m.trace(&mut self.store)
    }

    /// Everything processes wrote to stdout and stderr.
    pub fn output(&self) -> String {
        String::from_utf8_lossy(&self.store.data().t.stdout).into_owned()
    }

    /// The engine this world runs on.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Crashes so far.
    pub fn crashes(&self) -> u32 {
        self.store.data().t.crashes
    }

    /// The programs this world can spawn.
    pub fn programs(&self) -> &[Program] {
        &self.programs
    }
}

/// Shared ownership of compiled pieces, for forking many worlds.
pub type Programs = Arc<Vec<Program>>;
