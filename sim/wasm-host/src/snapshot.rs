//! Snapshots of a whole world: every linear memory (the machine's and each
//! process's) as content-hashed 4 KiB pages, every mutable global, and the
//! host's tables. Taken between steps, when every thread is suspended, so the
//! stacks Asyncify unwound are in the memories too.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use hegel_sim_core::Choices;
use serde::{Deserialize, Serialize};
use wasmtime::{AsContextMut, Engine, Extern, Instance, Memory, Val};

use crate::machine::Machine;
use crate::world::{HostTables, MachineModule, Program, WasmConfig, World, instantiate_live};

/// Page size snapshots hash at.
pub const PAGE: usize = 4096;

type Hash = [u8; 32];

/// Pages shared between snapshots (copy-on-write: a page is stored once
/// however many snapshots hold it).
#[derive(Clone, Default)]
pub struct PageStore {
    pages: Arc<Mutex<HashMap<Hash, Arc<[u8]>>>>,
}

impl PageStore {
    fn put(&self, page: &[u8]) -> Hash {
        let h = *blake3::hash(page).as_bytes();
        let mut g = self.pages.lock().unwrap_or_else(|e| e.into_inner());
        g.entry(h).or_insert_with(|| Arc::from(page));
        h
    }

    fn get(&self, h: &Hash) -> Result<Arc<[u8]>> {
        let g = self.pages.lock().unwrap_or_else(|e| e.into_inner());
        g.get(h)
            .cloned()
            .ok_or_else(|| anyhow!("hegel-sim-wasm: page {} is not in the page store", hex(h)))
    }

    /// Distinct pages held.
    pub fn len(&self) -> usize {
        self.pages.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    /// No pages held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn hex(h: &Hash) -> String {
    h.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct MemImage {
    len: u64,
    pages: Vec<Hash>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
enum GlobalVal {
    I32(i32),
    I64(i64),
    F32(u32),
    F64(u64),
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct InstanceImage {
    module: Hash,
    memory: MemImage,
    globals: BTreeMap<String, GlobalVal>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Manifest {
    machine: InstanceImage,
    procs: BTreeMap<u32, InstanceImage>,
    host: String,
    dump: String,
}

/// A whole world at one point. Cheap to hold many: pages are shared.
#[derive(Clone)]
pub struct Snapshot {
    manifest: Manifest,
    id: Hash,
    store: PageStore,
}

fn image(
    mut cx: impl AsContextMut,
    store: &PageStore,
    instance: Instance,
    memory: Memory,
    module: Hash,
) -> InstanceImage {
    let mut cx = cx.as_context_mut();
    let data = memory.data(&cx);
    let pages = data.chunks(PAGE).map(|p| store.put(p)).collect();
    let len = data.len() as u64;
    let exports: Vec<(String, Extern)> = instance
        .exports(&mut cx)
        .map(|e| (e.name().to_owned(), e.into_extern()))
        .collect();
    let mut globals = BTreeMap::new();
    for (name, e) in exports {
        let Extern::Global(g) = e else { continue };
        if g.ty(&cx).mutability() != wasmtime::Mutability::Var {
            continue;
        }
        let v = match g.get(&mut cx) {
            Val::I32(v) => GlobalVal::I32(v),
            Val::I64(v) => GlobalVal::I64(v),
            Val::F32(v) => GlobalVal::F32(v),
            Val::F64(v) => GlobalVal::F64(v),
            _ => continue,
        };
        globals.insert(name, v);
    }
    InstanceImage {
        module,
        memory: MemImage { len, pages },
        globals,
    }
}

fn restore_image(
    mut cx: impl AsContextMut,
    store: &PageStore,
    instance: Instance,
    memory: Memory,
    img: &InstanceImage,
) -> Result<()> {
    let mut cx = cx.as_context_mut();
    let have = memory.data_size(&cx) as u64;
    if have > img.memory.len {
        bail!(
            "hegel-sim-wasm: a fresh instance has {have} bytes of memory, more than the snapshot's {}",
            img.memory.len
        );
    }
    memory.grow(&mut cx, (img.memory.len - have) / 65536)?;
    let data = memory.data_mut(&mut cx);
    for (i, h) in img.memory.pages.iter().enumerate() {
        let p = store.get(h)?;
        data[i * PAGE..i * PAGE + p.len()].copy_from_slice(&p);
    }
    for (name, v) in &img.globals {
        let g = instance
            .get_global(&mut cx, name)
            .ok_or_else(|| anyhow!("hegel-sim-wasm: the module has no global export {name}"))?;
        let v = match *v {
            GlobalVal::I32(v) => Val::I32(v),
            GlobalVal::I64(v) => Val::I64(v),
            GlobalVal::F32(v) => Val::F32(v),
            GlobalVal::F64(v) => Val::F64(v),
        };
        g.set(&mut cx, v)?;
    }
    Ok(())
}

impl Snapshot {
    /// Content hash of the whole world (memories, globals, host tables).
    pub fn id(&self) -> String {
        hex(&self.id)
    }

    /// The machine's dump (files on disk and in cache, threads, clock) at the
    /// snapshot.
    pub fn dump(&self) -> &str {
        &self.manifest.dump
    }

    /// What differs between two snapshots: pages per memory, host tables, and
    /// the machine dump's changed lines.
    pub fn diff(&self, other: &Snapshot) -> String {
        let mut out = String::new();
        let mem = |name: &str,
                   a: Option<&InstanceImage>,
                   b: Option<&InstanceImage>,
                   out: &mut String| match (a, b) {
            (Some(a), Some(b)) => {
                let n = a.memory.pages.len().max(b.memory.pages.len());
                let changed = (0..n)
                    .filter(|i| a.memory.pages.get(*i) != b.memory.pages.get(*i))
                    .count();
                let globals = a
                    .globals
                    .iter()
                    .filter(|(k, v)| b.globals.get(*k) != Some(*v))
                    .count();
                if changed > 0 || globals > 0 {
                    out.push_str(&format!(
                        "{name}: {changed} of {n} pages and {globals} globals differ\n"
                    ));
                }
            }
            (Some(_), None) => out.push_str(&format!("{name}: only in the first\n")),
            (None, Some(_)) => out.push_str(&format!("{name}: only in the second\n")),
            (None, None) => {}
        };
        mem(
            "machine",
            Some(&self.manifest.machine),
            Some(&other.manifest.machine),
            &mut out,
        );
        let pids: std::collections::BTreeSet<u32> = self
            .manifest
            .procs
            .keys()
            .chain(other.manifest.procs.keys())
            .copied()
            .collect();
        for p in pids {
            mem(
                &format!("process {p}"),
                self.manifest.procs.get(&p),
                other.manifest.procs.get(&p),
                &mut out,
            );
        }
        if self.manifest.host != other.manifest.host {
            out.push_str("host tables differ\n");
        }
        let a: std::collections::BTreeSet<&str> = self.manifest.dump.lines().collect();
        let b: std::collections::BTreeSet<&str> = other.manifest.dump.lines().collect();
        for l in a.difference(&b) {
            out.push_str(&format!("- {l}\n"));
        }
        for l in b.difference(&a) {
            out.push_str(&format!("+ {l}\n"));
        }
        out
    }

    /// Write the snapshot to `dir`: `manifest.json` and `pages/<blake3>`.
    pub fn save(&self, dir: &Path) -> Result<()> {
        let pages = dir.join("pages");
        std::fs::create_dir_all(&pages)?;
        let all = std::iter::once(&self.manifest.machine).chain(self.manifest.procs.values());
        for img in all {
            for h in &img.memory.pages {
                let path = pages.join(hex(h));
                if !path.exists() {
                    std::fs::write(&path, self.store.get(h)?)?;
                }
            }
        }
        std::fs::write(
            dir.join("manifest.json"),
            serde_json::to_vec(&self.manifest)?,
        )?;
        Ok(())
    }

    /// Read a snapshot `save` wrote, checking every page against its hash.
    pub fn load(dir: &Path, store: &PageStore) -> Result<Snapshot> {
        let bytes = std::fs::read(dir.join("manifest.json"))
            .with_context(|| format!("hegel-sim-wasm: reading {}", dir.display()))?;
        let manifest: Manifest = serde_json::from_slice(&bytes)?;
        let all = std::iter::once(&manifest.machine).chain(manifest.procs.values());
        for img in all {
            for h in &img.memory.pages {
                let p = std::fs::read(dir.join("pages").join(hex(h)))?;
                if blake3::hash(&p).as_bytes() != h {
                    bail!(
                        "hegel-sim-wasm: page {} in {} does not match its hash",
                        hex(h),
                        dir.display()
                    );
                }
                store.put(&p);
            }
        }
        Ok(Snapshot {
            id: *blake3::hash(&bytes).as_bytes(),
            manifest,
            store: store.clone(),
        })
    }
}

impl<'c> World<'c> {
    /// Snapshot the whole world. Call between steps (after `run_until`
    /// returned `None`, or before the first run).
    pub fn snapshot(&mut self, store: &PageStore) -> Result<Snapshot> {
        let m = self
            .store
            .data()
            .machine
            .clone()
            .ok_or_else(|| anyhow!("hegel-sim-wasm: no machine"))?;
        let machine = image(
            &mut self.store,
            store,
            m.instance,
            m.memory,
            self.machine_module.hash,
        );
        let dump = m.dump(&mut self.store)?;
        restore_image(&mut self.store, store, m.instance, m.memory, &machine)?;
        let mut procs = BTreeMap::new();
        let live: Vec<(u32, crate::world::Live)> = self
            .store
            .data()
            .live
            .iter()
            .map(|(p, l)| (*p, l.clone()))
            .collect();
        for (pid, l) in live {
            let program = self.store.data().t.procs[&pid].program;
            procs.insert(
                pid,
                image(
                    &mut self.store,
                    store,
                    l.instance,
                    l.memory,
                    self.programs[program].hash,
                ),
            );
        }
        let host = serde_json::to_string(&self.store.data().t)?;
        let manifest = Manifest {
            machine,
            procs,
            host,
            dump,
        };
        let id = *blake3::hash(&serde_json::to_vec(&manifest)?).as_bytes();
        Ok(Snapshot {
            manifest,
            id,
            store: store.clone(),
        })
    }

    /// A world restored from `snap`, drawing its future from `choices`.
    pub fn restore(
        engine: &Engine,
        machine: &MachineModule,
        programs: Vec<Program>,
        cfg: WasmConfig,
        snap: &Snapshot,
        choices: &'c dyn Choices,
    ) -> Result<World<'c>> {
        if snap.manifest.machine.module != machine.hash {
            bail!("hegel-sim-wasm: the snapshot was taken with a different machine module");
        }
        let mut w = World::empty(engine, machine, programs, cfg, choices)?;
        let tables: HostTables = serde_json::from_str(&snap.manifest.host)?;
        let instance = w.linker.instantiate(&mut w.store, &machine.module)?;
        let memory = instance
            .get_memory(&mut w.store, "memory")
            .ok_or_else(|| anyhow!("hegel-sim-wasm: the machine module exports no memory"))?;
        restore_image(
            &mut w.store,
            &snap.store,
            instance,
            memory,
            &snap.manifest.machine,
        )?;
        let m = Machine::new(&mut w.store, instance)?;
        w.store.data_mut().machine = Some(m);
        for (pid, img) in &snap.manifest.procs {
            let p = tables.procs.get(pid).ok_or_else(|| {
                anyhow!("hegel-sim-wasm: the snapshot has memory for process {pid} but no table")
            })?;
            let program = w
                .programs
                .get(p.program)
                .ok_or_else(|| {
                    anyhow!("hegel-sim-wasm: the snapshot needs program #{}", p.program)
                })?
                .clone();
            if program.hash != img.module {
                bail!(
                    "hegel-sim-wasm: process {pid} ran a different build of {} than program #{}",
                    p.name,
                    p.program
                );
            }
            let live = instantiate_live(&mut w.linker, &mut w.store, &program.module)?;
            restore_image(&mut w.store, &snap.store, live.instance, live.memory, img)?;
            w.store.data_mut().live.insert(*pid, live);
        }
        w.store.data_mut().t = tables;
        Ok(w)
    }
}
