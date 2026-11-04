//! Load-time rewriting of a program module: a scheduler yield before every
//! atomic instruction, host calls in place of `memory.atomic.wait`/`notify`,
//! Asyncify so any switch-point import can unwind the wasm stack into linear
//! memory, and an export for every mutable global so a snapshot can save and
//! restore them.

use anyhow::{Context, Result, bail};
use walrus::ir::{self, Instr, InstrSeq, LoadKind, StoreKind, VisitorMut};
use walrus::{FunctionId, Module, ValType};

/// Prefix of the export names given to mutable globals.
pub const GLOBAL_EXPORT_PREFIX: &str = "hegel_global_";

/// Imports that are switch points (Asyncify may unwind through them).
pub const SWITCH_IMPORTS: &[(&str, &str)] = &[
    ("hegel", "yield"),
    ("hegel", "wait32"),
    ("hegel", "wait64"),
    ("wasi_snapshot_preview1", "fd_read"),
    ("wasi_snapshot_preview1", "fd_pread"),
    ("wasi_snapshot_preview1", "fd_write"),
    ("wasi_snapshot_preview1", "fd_pwrite"),
    ("wasi_snapshot_preview1", "fd_seek"),
    ("wasi_snapshot_preview1", "fd_tell"),
    ("wasi_snapshot_preview1", "fd_sync"),
    ("wasi_snapshot_preview1", "fd_datasync"),
    ("wasi_snapshot_preview1", "fd_close"),
    ("wasi_snapshot_preview1", "fd_filestat_get"),
    ("wasi_snapshot_preview1", "fd_filestat_set_size"),
    ("wasi_snapshot_preview1", "fd_readdir"),
    ("wasi_snapshot_preview1", "path_open"),
    ("wasi_snapshot_preview1", "path_filestat_get"),
    ("wasi_snapshot_preview1", "path_rename"),
    ("wasi_snapshot_preview1", "path_unlink_file"),
    ("wasi_snapshot_preview1", "path_create_directory"),
    ("wasi_snapshot_preview1", "path_remove_directory"),
    ("wasi_snapshot_preview1", "poll_oneoff"),
    ("wasi_snapshot_preview1", "sched_yield"),
];

struct Rewrite {
    yield_fn: FunctionId,
    wait32: FunctionId,
    wait64: FunctionId,
    notify: FunctionId,
    atomics: usize,
}

fn is_atomic(i: &Instr) -> bool {
    match i {
        Instr::AtomicRmw(_)
        | Instr::Cmpxchg(_)
        | Instr::AtomicNotify(_)
        | Instr::AtomicWait(_)
        | Instr::AtomicFence(_) => true,
        Instr::Load(l) => matches!(
            l.kind,
            LoadKind::I32 { atomic: true }
                | LoadKind::I64 { atomic: true }
                | LoadKind::I32_8 {
                    kind: ir::ExtendedLoad::ZeroExtendAtomic
                }
                | LoadKind::I32_16 {
                    kind: ir::ExtendedLoad::ZeroExtendAtomic
                }
                | LoadKind::I64_8 {
                    kind: ir::ExtendedLoad::ZeroExtendAtomic
                }
                | LoadKind::I64_16 {
                    kind: ir::ExtendedLoad::ZeroExtendAtomic
                }
                | LoadKind::I64_32 {
                    kind: ir::ExtendedLoad::ZeroExtendAtomic
                }
        ),
        Instr::Store(s) => matches!(
            s.kind,
            StoreKind::I32 { atomic: true }
                | StoreKind::I64 { atomic: true }
                | StoreKind::I32_8 { atomic: true }
                | StoreKind::I32_16 { atomic: true }
                | StoreKind::I64_8 { atomic: true }
                | StoreKind::I64_16 { atomic: true }
                | StoreKind::I64_32 { atomic: true }
        ),
        _ => false,
    }
}

impl VisitorMut for Rewrite {
    fn start_instr_seq_mut(&mut self, seq: &mut InstrSeq) {
        if !seq.instrs.iter().any(|(i, _)| is_atomic(i)) {
            return;
        }
        let old = std::mem::take(&mut seq.instrs);
        for (instr, loc) in old {
            if !is_atomic(&instr) {
                seq.instrs.push((instr, loc));
                continue;
            }
            self.atomics += 1;
            seq.instrs.push((
                Instr::Call(ir::Call {
                    func: self.yield_fn,
                }),
                loc,
            ));
            match instr {
                Instr::AtomicWait(w) => {
                    seq.instrs.push((
                        Instr::Const(ir::Const {
                            value: ir::Value::I32(w.arg.offset as i32),
                        }),
                        loc,
                    ));
                    let func = if w.sixty_four {
                        self.wait64
                    } else {
                        self.wait32
                    };
                    seq.instrs.push((Instr::Call(ir::Call { func }), loc));
                }
                Instr::AtomicNotify(n) => {
                    seq.instrs.push((
                        Instr::Const(ir::Const {
                            value: ir::Value::I32(n.arg.offset as i32),
                        }),
                        loc,
                    ));
                    seq.instrs
                        .push((Instr::Call(ir::Call { func: self.notify }), loc));
                }
                other => seq.instrs.push((other, loc)),
            }
        }
    }
}

fn import_fn(m: &mut Module, name: &str, params: &[ValType], results: &[ValType]) -> FunctionId {
    let ty = m.types.add(params, results);
    m.add_import_func("hegel", name, ty).0
}

/// Insert the yields and wait/notify calls. Returns the rewritten module and
/// the number of atomic instructions instrumented.
pub fn insert_yields(wasm: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut m = Module::from_buffer(wasm).context("hegel-sim-wasm: parsing the program module")?;
    let mut rw = Rewrite {
        yield_fn: import_fn(&mut m, "yield", &[], &[]),
        wait32: import_fn(
            &mut m,
            "wait32",
            &[ValType::I32, ValType::I32, ValType::I64, ValType::I32],
            &[ValType::I32],
        ),
        wait64: import_fn(
            &mut m,
            "wait64",
            &[ValType::I32, ValType::I64, ValType::I64, ValType::I32],
            &[ValType::I32],
        ),
        notify: import_fn(
            &mut m,
            "notify",
            &[ValType::I32, ValType::I32, ValType::I32],
            &[ValType::I32],
        ),
        atomics: 0,
    };
    for (_, f) in m.funcs.iter_local_mut() {
        let entry = f.entry_block();
        ir::dfs_pre_order_mut(&mut rw, f, entry);
    }
    let n = rw.atomics;
    Ok((m.emit_wasm(), n))
}

/// Export every mutable global not already exported.
pub fn export_globals(wasm: &[u8]) -> Result<Vec<u8>> {
    let mut m = Module::from_buffer(wasm)
        .context("hegel-sim-wasm: parsing a module to export its globals")?;
    let exported: std::collections::BTreeSet<walrus::GlobalId> = m
        .exports
        .iter()
        .filter_map(|e| match e.item {
            walrus::ExportItem::Global(g) => Some(g),
            _ => None,
        })
        .collect();
    let globals: Vec<(usize, walrus::GlobalId)> = m
        .globals
        .iter()
        .enumerate()
        .filter(|(_, g)| g.mutable && !exported.contains(&g.id()))
        .map(|(i, g)| (i, g.id()))
        .collect();
    for (i, id) in globals {
        m.exports.add(&format!("{GLOBAL_EXPORT_PREFIX}{i}"), id);
    }
    Ok(m.emit_wasm())
}

/// Run binaryen's Asyncify over `wasm`, unwinding through every switch-point
/// import the module actually has. `optimize` also runs `-O2`, which is what
/// keeps Asyncify's overhead down; the measurement compares like with like.
pub fn asyncify(wasm: &[u8], optimize: bool) -> Result<Vec<u8>> {
    let m = Module::from_buffer(wasm).context("hegel-sim-wasm: parsing a module for Asyncify")?;
    let present: Vec<String> = m
        .imports
        .iter()
        .filter(|i| {
            SWITCH_IMPORTS
                .iter()
                .any(|(md, n)| *md == i.module && *n == i.name)
        })
        .map(|i| format!("{}.{}", i.module, i.name))
        .collect();
    if present.is_empty() {
        bail!(
            "hegel-sim-wasm: the module imports no switch point, so Asyncify has nothing to unwind through"
        );
    }
    run_wasm_opt(wasm, Some(&present.join(",")), optimize)
}

/// Run wasm-opt with `-O2` (when `optimize`) and, when `asyncify_imports` is
/// given, the Asyncify pass.
pub fn run_wasm_opt(
    wasm: &[u8],
    asyncify_imports: Option<&str>,
    optimize: bool,
) -> Result<Vec<u8>> {
    let dir = std::env::temp_dir().join(format!(
        "hegel-sim-wasm-{}-{}",
        std::process::id(),
        blake3::hash(wasm).to_hex()
    ));
    std::fs::create_dir_all(&dir)?;
    let input = dir.join("in.wasm");
    let output = dir.join("out.wasm");
    std::fs::write(&input, wasm)?;
    let mut opts = if optimize {
        wasm_opt::OptimizationOptions::new_opt_level_2()
    } else {
        wasm_opt::OptimizationOptions::new_opt_level_0()
    };
    opts.all_features();
    if let Some(imports) = asyncify_imports {
        opts.add_pass(wasm_opt::Pass::Asyncify);
        opts.set_pass_arg("asyncify-imports", imports);
    }
    opts.run(&input, &output)
        .map_err(|e| anyhow::anyhow!("hegel-sim-wasm: wasm-opt failed: {e}"))?;
    let out = std::fs::read(&output)?;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(out)
}

/// The whole pipeline for a program module under Asyncify.
pub fn prepare_program(wasm: &[u8]) -> Result<(Vec<u8>, usize)> {
    let (with_yields, atomics) = insert_yields(wasm)?;
    let asyncified = asyncify(&with_yields, true)?;
    Ok((export_globals(&asyncified)?, atomics))
}
