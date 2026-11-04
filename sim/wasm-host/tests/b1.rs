//! Bug 1 (no fsync before rename) as a real `wasm32-wasip1` program in a wasm
//! world: a calm setup runs once and is snapshot; every Hegel case restores
//! that snapshot, draws its own swarm, schedule and crash, and compacts. The
//! buggy build loses rows after power loss; the fixed one never does.
//!
//! Needs the machine and guest modules built first:
//! `cargo build -p hegel-sim-machine --release --target wasm32-unknown-unknown`
//! and `cargo build --release --target wasm32-wasip1` in `sim/wasm-guests`.

use std::path::PathBuf;
use std::sync::OnceLock;

use hegel::{HealthCheck, Hegel, Settings, TestCase};
use hegel_sim_core::{Fault, SeededChoices};
use hegel_sim_wasm::{
    MachineModule, Outcome, PageStore, Program, Snapshot, StackMode, WasmConfig, World, engine,
};

const INPUTS: [&str; 3] = ["/in/a", "/in/b", "/in/c"];

fn rows(i: usize) -> String {
    (0..40).map(|r| format!("row-{i}-{r:04}\n")).collect()
}

fn expected() -> String {
    (0..INPUTS.len()).map(rows).collect()
}

fn module(rel: &str, build: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}; build it with `{build}`", p.display()))
}

struct Fixture {
    engine: wasmtime::Engine,
    machine: MachineModule,
    program: Program,
    setup: Snapshot,
}

fn fixture() -> &'static Fixture {
    static F: OnceLock<Fixture> = OnceLock::new();
    F.get_or_init(|| {
        let engine = engine(StackMode::Asyncify).unwrap();
        let machine = MachineModule::new(
            &engine,
            &module(
                "target/wasm32-unknown-unknown/release/hegel_sim_machine.wasm",
                "cargo build -p hegel-sim-machine --release --target wasm32-unknown-unknown",
            ),
        )
        .unwrap();
        let program = Program::new(
            &engine,
            "b1",
            &module(
                "sim/wasm-guests/target/wasm32-wasip1/release/b1.wasm",
                "cargo build --release --target wasm32-wasip1 (in sim/wasm-guests)",
            ),
        )
        .unwrap();
        let calm = SeededChoices::new(0);
        let cfg = WasmConfig {
            swarm: false,
            ..WasmConfig::default()
        }
        .faults([])
        .max_crashes(0);
        let mut w = World::new(&engine, &machine, vec![program.clone()], cfg, &calm).unwrap();
        w.spawn(0, &["setup"]).unwrap();
        let out = w.run().unwrap();
        assert_eq!(
            out,
            Outcome::Exited(vec![(1, "b1".into(), 0)]),
            "setup failed:\n{}\n{}",
            w.output(),
            w.trace().unwrap()
        );
        let setup = w.snapshot(&PageStore::default()).unwrap();
        Fixture {
            engine,
            machine,
            program,
            setup,
        }
    })
}

fn recover(w: &mut World<'_>) -> Option<String> {
    let manifest = w
        .read_file("/out/manifest.json")
        .unwrap()
        .and_then(|m| String::from_utf8(m).ok());
    if let Some(m) = manifest
        && let Some(bytes) = m
            .strip_prefix("{\"segments\":[\"seg-1\"],\"bytes\":")
            .and_then(|r| r.strip_suffix('}'))
        && let Ok(n) = bytes.parse::<usize>()
    {
        let seg = w.read_file("/out/seg-1").unwrap()?;
        return (seg.len() == n).then(|| String::from_utf8_lossy(&seg).into_owned());
    }
    let mut out = String::new();
    for p in INPUTS {
        out.push_str(&String::from_utf8(w.read_file(p).unwrap()?).ok()?);
    }
    Some(out)
}

fn scenario(tc: &TestCase, variant: &str) {
    let f = fixture();
    let cfg = WasmConfig::default()
        .faults([Fault::PowerLoss, Fault::TornWrites, Fault::ZerosPastEof])
        .max_crashes(1);
    let mut w = World::restore(
        &f.engine,
        &f.machine,
        vec![f.program.clone()],
        cfg.clone(),
        &f.setup,
        tc,
    )
    .unwrap();
    w.reconfigure(cfg).unwrap();
    w.spawn(0, &["compact", variant]).unwrap();
    let out = w.run().unwrap();
    let got = recover(&mut w);
    assert!(
        got.as_deref() == Some(expected().as_str()),
        "rows lost after {out:?}: recovery read {} bytes, expected {}\n{}",
        got.map_or(0, |g| g.len()),
        expected().len(),
        w.trace().unwrap()
    );
}

fn first_failure(variant: &'static str, cases: u64, seed: u64) -> Option<u64> {
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::sync::atomic::{AtomicU64, Ordering};
    let count = AtomicU64::new(0);
    let first = AtomicU64::new(0);
    let settings = Settings::new()
        .test_cases(cases)
        .seed(Some(seed))
        .database(None)
        .suppress_health_check(HealthCheck::all());
    let _ = catch_unwind(AssertUnwindSafe(|| {
        Hegel::new(|tc: TestCase| {
            let n = count.fetch_add(1, Ordering::SeqCst) + 1;
            if let Err(p) = catch_unwind(AssertUnwindSafe(|| scenario(&tc, variant))) {
                let _ = first.compare_exchange(0, n, Ordering::SeqCst, Ordering::SeqCst);
                resume_unwind(p);
            }
        })
        .settings(settings)
        .run();
    }));
    let f = first.load(Ordering::SeqCst);
    (f != 0).then_some(f)
}

#[test]
fn b1_in_wasm_buggy_caught_fixed_passes() {
    let mut firsts = Vec::new();
    for seed in [1, 2, 3] {
        firsts.push(
            first_failure("buggy", 1000, seed)
                .unwrap_or_else(|| panic!("b1 buggy passed 1000 cases in wasm (seed {seed})")),
        );
        assert_eq!(
            first_failure("fixed", 1000, seed),
            None,
            "b1 fixed failed in wasm (seed {seed})"
        );
    }
    eprintln!(
        "b1 in wasm: buggy caught at case {firsts:?} (seeds [1, 2, 3]); fixed passed 1000 cases per seed"
    );
}

#[test]
fn fork_from_one_snapshot() {
    let f = fixture();
    let cfg = WasmConfig::default()
        .faults([Fault::PowerLoss, Fault::TornWrites])
        .max_crashes(1);
    let mut ids = std::collections::BTreeSet::new();
    let store = PageStore::default();
    for seed in 0..8 {
        let ch = SeededChoices::new(seed);
        let mut w = World::restore(
            &f.engine,
            &f.machine,
            vec![f.program.clone()],
            cfg.clone(),
            &f.setup,
            &ch,
        )
        .unwrap();
        w.reconfigure(cfg.clone()).unwrap();
        w.spawn(0, &["compact", "fixed"]).unwrap();
        w.run().unwrap();
        ids.insert(w.snapshot(&store).unwrap().id());
    }
    assert!(
        ids.len() > 1,
        "eight branches from one snapshot all ended in the same world"
    );
    let same = World::restore(
        &f.engine,
        &f.machine,
        vec![f.program.clone()],
        cfg.clone(),
        &f.setup,
        &SeededChoices::new(0),
    )
    .unwrap()
    .snapshot(&store)
    .unwrap();
    assert_eq!(
        same.id(),
        f.setup.id(),
        "a restore does not round-trip to the snapshot it came from:\n{}",
        same.diff(&f.setup)
    );
    let dir = std::env::temp_dir().join(format!("hegel-sim-wasm-snap-{}", std::process::id()));
    f.setup.save(&dir).unwrap();
    let loaded = Snapshot::load(&dir, &PageStore::default()).unwrap();
    assert_eq!(loaded.diff(&f.setup), "");
    let _ = std::fs::remove_dir_all(&dir);
}
