//! What Asyncify costs on a blaze-like workload: the same `wasm32-wasip1`
//! program optimised with `wasm-opt -O2` alone and with Asyncify (the phase 2a
//! pipeline), run on stub WASI that never switches, then the prepared module in
//! a real world (every syscall a switch point). Prints sizes and median times.
//!
//! `cargo run --release -p hegel-sim-wasm --example asyncify_overhead -- <workload.wasm> <machine.wasm> [n] [runs]`

use std::time::Duration;

use anyhow::{Result, anyhow};
use hegel_sim_core::SeededChoices;
use hegel_sim_wasm::{MachineModule, Program, StackMode, WasmConfig, World, engine, instrument};
use wasmtime::{Caller, Engine, Extern, Func, Linker, Module, Store, Val};

fn mem(c: &mut Caller<'_, ()>) -> Result<wasmtime::Memory> {
    c.get_export("memory")
        .and_then(Extern::into_memory)
        .ok_or_else(|| anyhow!("no memory"))
}

fn stub_linker(
    engine: &Engine,
    store: &mut Store<()>,
    module: &Module,
    n: &str,
) -> Result<Linker<()>> {
    let mut l: Linker<()> = Linker::new(engine);
    const W: &str = "wasi_snapshot_preview1";
    let args = ["workload".to_owned(), n.to_owned()];
    let a1 = args.clone();
    l.func_wrap(
        W,
        "args_sizes_get",
        move |mut c: Caller<'_, ()>, argc: u32, size: u32| -> Result<i32> {
            let m = mem(&mut c)?;
            m.write(&mut c, argc as usize, &(a1.len() as u32).to_le_bytes())?;
            m.write(
                &mut c,
                size as usize,
                &(a1.iter().map(|a| a.len() as u32 + 1).sum::<u32>()).to_le_bytes(),
            )?;
            Ok(0)
        },
    )?;
    l.func_wrap(
        W,
        "args_get",
        move |mut c: Caller<'_, ()>, argv: u32, buf: u32| -> Result<i32> {
            let m = mem(&mut c)?;
            let mut at = buf;
            for (i, a) in args.iter().enumerate() {
                m.write(&mut c, argv as usize + i * 4, &at.to_le_bytes())?;
                m.write(&mut c, at as usize, a.as_bytes())?;
                m.write(&mut c, at as usize + a.len(), &[0])?;
                at += a.len() as u32 + 1;
            }
            Ok(0)
        },
    )?;
    l.func_wrap(
        W,
        "fd_write",
        |mut c: Caller<'_, ()>, _fd: u32, iovs: u32, n: u32, out: u32| -> Result<i32> {
            let m = mem(&mut c)?;
            let d = m.data(&c);
            let total: u32 = (0..n)
                .map(|i| {
                    u32::from_le_bytes(
                        d[(iovs + i * 8 + 4) as usize..(iovs + i * 8 + 8) as usize]
                            .try_into()
                            .unwrap_or_default(),
                    )
                })
                .sum();
            m.write(&mut c, out as usize, &total.to_le_bytes())?;
            Ok(0)
        },
    )?;
    l.func_wrap(
        W,
        "path_open",
        |mut c: Caller<'_, ()>,
         _: u32,
         _: u32,
         _: u32,
         _: u32,
         _: u32,
         _: u64,
         _: u64,
         _: u32,
         out: u32|
         -> Result<i32> {
            mem(&mut c)?.write(&mut c, out as usize, &4u32.to_le_bytes())?;
            Ok(0)
        },
    )?;
    l.func_wrap(
        W,
        "fd_prestat_get",
        |mut c: Caller<'_, ()>, fd: u32, out: u32| -> Result<i32> {
            if fd != 3 {
                return Ok(8);
            }
            let m = mem(&mut c)?;
            m.write(&mut c, out as usize, &[0, 0, 0, 0, 1, 0, 0, 0])?;
            Ok(0)
        },
    )?;
    l.func_wrap(
        W,
        "fd_prestat_dir_name",
        |mut c: Caller<'_, ()>, _: u32, p: u32, _: u32| -> Result<i32> {
            mem(&mut c)?.write(&mut c, p as usize, b"/")?;
            Ok(0)
        },
    )?;
    l.func_wrap(
        W,
        "random_get",
        |mut c: Caller<'_, ()>, p: u32, len: u32| -> Result<i32> {
            mem(&mut c)?.write(&mut c, p as usize, &vec![7u8; len as usize])?;
            Ok(0)
        },
    )?;
    l.func_wrap(
        W,
        "proc_exit",
        |_: Caller<'_, ()>, code: i32| -> Result<()> { Err(anyhow!("exit {code}")) },
    )?;
    for imp in module.imports() {
        if l.get(&mut *store, imp.module(), imp.name()).is_some() {
            continue;
        }
        let wasmtime::ExternType::Func(ft) = imp.ty() else {
            continue;
        };
        let f = Func::new(&mut *store, ft.clone(), |_, _, results| {
            for r in results.iter_mut() {
                *r = Val::I32(0);
            }
            Ok(())
        });
        l.define(&mut *store, imp.module(), imp.name(), Extern::Func(f))?;
    }
    Ok(l)
}

/// This thread's CPU time: the Mac this runs on is shared, and wall time
/// swings with its load.
fn cpu() -> Duration {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid out-pointer for the duration of the call.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn run_stub(engine: &Engine, wasm: &[u8], n: &str, runs: usize) -> Result<Duration> {
    let module = Module::new(engine, wasm)?;
    let mut times = Vec::new();
    for _ in 0..runs {
        let mut store = Store::new(engine, ());
        let linker = stub_linker(engine, &mut store, &module, n)?;
        let inst = linker.instantiate(&mut store, &module)?;
        let start = inst.get_typed_func::<(), ()>(&mut store, "_start")?;
        let t = cpu();
        start.call(&mut store, ())?;
        times.push(cpu() - t);
    }
    Ok(median(times))
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, workload, machine, ..] = args.as_slice() else {
        return Err(anyhow!(
            "usage: asyncify_overhead <workload.wasm> <machine.wasm> [n] [runs]"
        ));
    };
    let n = args.get(3).cloned().unwrap_or_else(|| "200000".into());
    let runs: usize = args.get(4).map_or(Ok(21), |s| s.parse())?;
    let raw = std::fs::read(workload)?;
    let engine = engine(StackMode::Asyncify)?;
    let plain = instrument::run_wasm_opt(&raw, None, true)?;
    let (prepared, atomics) = instrument::prepare_program(&raw)?;
    let (mut tp, mut ta) = (Vec::new(), Vec::new());
    for _ in 0..runs {
        tp.push(run_stub(&engine, &plain, &n, 1)?);
        ta.push(run_stub(&engine, &prepared, &n, 1)?);
    }
    eprintln!("plain runs: {tp:.1?}\nasyncify runs: {ta:.1?}");
    let (t_plain, t_asy) = (median(tp), median(ta));
    let mm = MachineModule::new(&engine, &std::fs::read(machine)?)?;
    let program = Program::new(&engine, "workload", &raw)?;
    let mut world_times = Vec::new();
    let mut out = String::new();
    for _ in 0..runs {
        let calm = SeededChoices::new(0);
        let cfg = WasmConfig {
            swarm: false,
            max_depth: 0,
            ..WasmConfig::default()
        }
        .faults([])
        .max_crashes(0);
        let mut w = World::new(&engine, &mm, vec![program.clone()], cfg, &calm)?;
        w.spawn(0, &[&n])?;
        let t = cpu();
        w.run()?;
        world_times.push(cpu() - t);
        out = w.output();
    }
    let t_world = median(world_times);
    println!(
        "workload n={n}, median thread CPU time of {runs} runs, wasmtime 38 cranelift, atomics instrumented: {atomics}"
    );
    println!("raw module:        {:>8} bytes", raw.len());
    println!(
        "wasm-opt -O2:      {:>8} bytes  {:>10.2?}",
        plain.len(),
        t_plain
    );
    println!(
        "-O2 + Asyncify:    {:>8} bytes  {:>10.2?}  (size x{:.2}, time x{:.2})",
        prepared.len(),
        t_asy,
        prepared.len() as f64 / plain.len() as f64,
        t_asy.as_secs_f64() / t_plain.as_secs_f64()
    );
    println!(
        "in a world:                       {:>10.2?}  (time x{:.2} over -O2)",
        t_world,
        t_world.as_secs_f64() / t_plain.as_secs_f64()
    );
    println!("world stdout: {}", out.trim());
    Ok(())
}
