//! Typed handles on one machine instance (the kernel model as a wasm module)
//! and the scratch-buffer protocol its exports use.

use anyhow::{Context, Result, anyhow};
use hegel_sim_core::kernel::wire;
use hegel_sim_core::{SysResult, Syscall};
use wasmtime::{AsContext, AsContextMut, Instance, Memory, TypedFunc};

/// The machine module's exports.
#[derive(Clone)]
pub(crate) struct Machine {
    pub(crate) instance: Instance,
    pub(crate) memory: Memory,
    buf: u32,
    init: TypedFunc<u32, i32>,
    reconfigure: TypedFunc<u32, i32>,
    proc_spawn: TypedFunc<u32, ()>,
    proc_exit: TypedFunc<u32, ()>,
    thread_spawn: TypedFunc<u32, u32>,
    thread_exit: TypedFunc<u32, ()>,
    syscall: TypedFunc<(u32, u32, u32), i32>,
    sleep_until: TypedFunc<(u32, u64), ()>,
    yielded: TypedFunc<u32, ()>,
    pick: TypedFunc<(), i32>,
    begin_epoch: TypedFunc<(), ()>,
    maybe_crash: TypedFunc<u32, i32>,
    crash: TypedFunc<(u32, u32), ()>,
    now_ns: TypedFunc<(), u64>,
    step: TypedFunc<(), u64>,
    trace: TypedFunc<(), u32>,
    read_file: TypedFunc<u32, i32>,
    dump: TypedFunc<(), u32>,
    last_len: TypedFunc<(), u32>,
}

/// What `maybe_crash` drew (mirrors `CrashKind`).
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Crash {
    None,
    Kill(u32),
    PowerLoss,
    OsCrash,
}

impl Machine {
    pub(crate) fn new(mut cx: impl AsContextMut, instance: Instance) -> Result<Machine> {
        let mut cx = cx.as_context_mut();
        macro_rules! f {
            ($name:literal) => {
                instance.get_typed_func(&mut cx, $name).with_context(|| {
                    format!(
                        "hegel-sim-wasm: the machine module lacks export `{}`",
                        $name
                    )
                })?
            };
        }
        let memory = instance
            .get_memory(&mut cx, "memory")
            .ok_or_else(|| anyhow!("hegel-sim-wasm: the machine module exports no memory"))?;
        let buf_ptr: TypedFunc<(), u32> = f!("buf_ptr");
        let buf = buf_ptr.call(&mut cx, ())?;
        Ok(Machine {
            instance,
            memory,
            buf,
            init: f!("init"),
            reconfigure: f!("reconfigure"),
            proc_spawn: f!("proc_spawn"),
            proc_exit: f!("proc_exit"),
            thread_spawn: f!("thread_spawn"),
            thread_exit: f!("thread_exit"),
            syscall: f!("syscall"),
            sleep_until: f!("sleep_until"),
            yielded: f!("yielded"),
            pick: f!("pick"),
            begin_epoch: f!("begin_epoch"),
            maybe_crash: f!("maybe_crash"),
            crash: f!("crash"),
            now_ns: f!("now_ns"),
            step: f!("step"),
            trace: f!("trace"),
            read_file: f!("read_file"),
            dump: f!("dump"),
            last_len: f!("last_len"),
        })
    }

    fn put(&self, mut cx: impl AsContextMut, bytes: &[u8]) -> Result<u32> {
        self.memory.write(&mut cx, self.buf as usize, bytes)?;
        Ok(bytes.len() as u32)
    }

    fn get(&self, cx: impl AsContext, len: u32) -> Vec<u8> {
        let data = self.memory.data(&cx);
        data[self.buf as usize..self.buf as usize + len as usize].to_vec()
    }

    pub(crate) fn init(&self, mut cx: impl AsContextMut, cfg: &[u8]) -> Result<()> {
        let n = self.put(&mut cx, cfg)?;
        if self.init.call(&mut cx, n)? < 0 {
            let len = self.last_len.call(&mut cx, ())?;
            return Err(anyhow!(
                "hegel-sim-wasm: the machine refused its config: {}",
                String::from_utf8_lossy(&self.get(&cx, len))
            ));
        }
        Ok(())
    }

    pub(crate) fn reconfigure(&self, mut cx: impl AsContextMut, cfg: &[u8]) -> Result<()> {
        let n = self.put(&mut cx, cfg)?;
        if self.reconfigure.call(&mut cx, n)? < 0 {
            let len = self.last_len.call(&mut cx, ())?;
            return Err(anyhow!(
                "hegel-sim-wasm: {}",
                String::from_utf8_lossy(&self.get(&cx, len))
            ));
        }
        Ok(())
    }

    pub(crate) fn proc_spawn(&self, cx: impl AsContextMut, pid: u32) -> Result<()> {
        self.proc_spawn.call(cx, pid)
    }

    pub(crate) fn proc_exit(&self, cx: impl AsContextMut, pid: u32) -> Result<()> {
        self.proc_exit.call(cx, pid)
    }

    pub(crate) fn thread_spawn(&self, cx: impl AsContextMut, pid: u32) -> Result<u32> {
        self.thread_spawn.call(cx, pid)
    }

    pub(crate) fn thread_exit(&self, cx: impl AsContextMut, tid: u32) -> Result<()> {
        self.thread_exit.call(cx, tid)
    }

    pub(crate) fn syscall(
        &self,
        mut cx: impl AsContextMut,
        pid: u32,
        tid: u32,
        call: &Syscall,
    ) -> Result<SysResult> {
        let mut bytes = Vec::new();
        wire::encode_syscall(call, &mut bytes);
        let n = self.put(&mut cx, &bytes)?;
        let len = self.syscall.call(&mut cx, (pid, tid, n))?;
        if len < 0 {
            let n = self.last_len.call(&mut cx, ())?;
            return Err(anyhow!(
                "hegel-sim-wasm: the machine refused syscall {}: {}",
                call.name(),
                String::from_utf8_lossy(&self.get(&cx, n))
            ));
        }
        let out = self.get(&cx, len as u32);
        wire::decode_result(&out).map_err(|e| {
            anyhow!(
                "hegel-sim-wasm: decoding the result of {}: {e:?}",
                call.name()
            )
        })
    }

    pub(crate) fn sleep_until(&self, cx: impl AsContextMut, tid: u32, deadline: u64) -> Result<()> {
        self.sleep_until.call(cx, (tid, deadline))
    }

    pub(crate) fn yielded(&self, cx: impl AsContextMut, tid: u32) -> Result<()> {
        self.yielded.call(cx, tid)
    }

    /// The next thread to run; `Ok(None)` when every thread is done; an error
    /// naming every blocked thread on deadlock.
    pub(crate) fn pick(&self, mut cx: impl AsContextMut) -> Result<Option<u32>> {
        match self.pick.call(&mut cx, ())? {
            -2 => Ok(None),
            -3 => {
                let n = self.last_len.call(&mut cx, ())?;
                let what = String::from_utf8_lossy(&self.get(&cx, n)).into_owned();
                Err(anyhow!(
                    "hegel-sim-wasm: deadlock: every live thread is blocked and no timer is pending:\n{what}\n{}",
                    self.trace(&mut cx)?
                ))
            }
            t if t >= 0 => Ok(Some(t as u32)),
            t => Err(anyhow!("hegel-sim-wasm: the machine's pick returned {t}")),
        }
    }

    pub(crate) fn begin_epoch(&self, cx: impl AsContextMut) -> Result<()> {
        self.begin_epoch.call(cx, ())
    }

    pub(crate) fn maybe_crash(&self, mut cx: impl AsContextMut, at_syscall: bool) -> Result<Crash> {
        Ok(
            match self.maybe_crash.call(&mut cx, u32::from(at_syscall))? {
                0 => Crash::None,
                1 => {
                    let b = self.get(&cx, 4);
                    Crash::Kill(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                }
                2 => Crash::PowerLoss,
                3 => Crash::OsCrash,
                k => {
                    return Err(anyhow!(
                        "hegel-sim-wasm: the machine drew unknown crash kind {k}"
                    ));
                }
            },
        )
    }

    pub(crate) fn crash(&self, cx: impl AsContextMut, c: Crash) -> Result<()> {
        let (kind, pid) = match c {
            Crash::None => return Ok(()),
            Crash::Kill(p) => (1, p),
            Crash::PowerLoss => (2, 0),
            Crash::OsCrash => (3, 0),
        };
        self.crash.call(cx, (kind, pid))
    }

    pub(crate) fn now_ns(&self, cx: impl AsContextMut) -> Result<u64> {
        self.now_ns.call(cx, ())
    }

    pub(crate) fn step(&self, cx: impl AsContextMut) -> Result<u64> {
        self.step.call(cx, ())
    }

    pub(crate) fn trace(&self, mut cx: impl AsContextMut) -> Result<String> {
        let n = self.trace.call(&mut cx, ())?;
        Ok(String::from_utf8_lossy(&self.get(&cx, n)).into_owned())
    }

    pub(crate) fn read_file(
        &self,
        mut cx: impl AsContextMut,
        path: &str,
    ) -> Result<Option<Vec<u8>>> {
        let n = self.put(&mut cx, path.as_bytes())?;
        let len = self.read_file.call(&mut cx, n)?;
        match len {
            -1 => Ok(None),
            -2 => Err(anyhow!(
                "hegel-sim-wasm: {path} is larger than the machine's 1 MiB buffer"
            )),
            n => Ok(Some(self.get(&cx, n as u32))),
        }
    }

    pub(crate) fn dump(&self, mut cx: impl AsContextMut) -> Result<String> {
        let n = self.dump.call(&mut cx, ())?;
        Ok(String::from_utf8_lossy(&self.get(&cx, n)).into_owned())
    }
}
