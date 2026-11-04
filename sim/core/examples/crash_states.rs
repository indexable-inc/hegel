//! The kernel model's membership checker for `sim/validation`: is a crash state
//! observed on a real file system one the model can produce?
//!
//! `crash_states <workload.json>` with the observed state on stdin (the JSON
//! `validate_kernel_model.py` sends), prints one line
//! `{"allowed": bool, "reason": str, "rules": [str]}`.
//!
//! `crash_states --self-test <workloads dir> [seeds]` checks the checker
//! against the model itself: every state the model's own crash draws (each
//! workload, each op prefix, each file system mapping, `seeds` draws) must be
//! allowed.
//!
//! The workload's ops before the crash are deterministic in the model (no
//! syscall faults are on), so the search is over the crash's own fates only:
//! which pending directory operations survive, and per file the size and each
//! page's version or tear. `Kernel::could_crash_to` answers that exactly.

use std::collections::BTreeMap;
use std::io::Read;

use hegel_sim_core::kernel::{CrashKind, Fd, KernelConfig, Observed, OpenFlags, Profile};
use hegel_sim_core::{Fault, FaultSet, Kernel, Rates, SeededChoices, Swarm, SysOk, SysResult, Syscall, Trace};
use serde_json::{Value, json};

fn field<'a>(v: &'a Value, k: &str, at: &str) -> Result<&'a Value, String> {
    v.get(k).ok_or_else(|| format!("{at}: missing field {k:?}"))
}

fn text<'a>(v: &'a Value, k: &str, at: &str) -> Result<&'a str, String> {
    field(v, k, at)?.as_str().ok_or_else(|| format!("{at}.{k}: expected a string"))
}

fn num(v: &Value, k: &str, at: &str) -> Result<u64, String> {
    field(v, k, at)?.as_u64().ok_or_else(|| format!("{at}.{k}: expected a non-negative integer"))
}

fn b64(s: &str) -> Result<Vec<u8>, String> {
    let val = |c: u8| -> Result<u32, String> {
        Ok(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return Err(format!("content_b64: byte {c:#x} is not base64")),
        } as u32)
    };
    let bytes = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            n |= val(*c)? << (18 - 6 * i);
        }
        out.extend_from_slice(&n.to_be_bytes()[1..chunk.len()]);
    }
    Ok(out)
}

/// The model's profile and crash-shape classes for each file system the
/// validation config names.
fn model_for(fs: &str) -> Result<(Profile, FaultSet, &'static str), String> {
    let torn = [Fault::TornWrites];
    let zeros = [Fault::TornWrites, Fault::ZerosPastEof];
    Ok(match fs {
        "ext4-ordered" => (Profile::LinuxExt4, torn.into_iter().collect(), "ext4 data=ordered: LinuxExt4, torn sectors"),
        "ext4-writeback" => (Profile::LinuxExt4, zeros.into_iter().collect(), "ext4 data=writeback: LinuxExt4, torn sectors, zeros past EOF"),
        "xfs" => (Profile::LinuxStrict, zeros.into_iter().collect(), "xfs: LinuxStrict, torn sectors, zeros past EOF"),
        "btrfs" => (Profile::LinuxStrict, torn.into_iter().collect(), "btrfs: LinuxStrict, torn sectors"),
        other => return Err(format!("fs {other:?} has no model mapping (known: ext4-ordered, ext4-writeback, xfs, btrfs)")),
    })
}

fn open_flags(v: &Value, at: &str) -> Result<OpenFlags, String> {
    let mut f = OpenFlags::default();
    for fl in field(v, "flags", at)?.as_array().ok_or_else(|| format!("{at}.flags: expected a list"))? {
        match fl.as_str() {
            Some("read") => f.read = true,
            Some("write") => f.write = true,
            Some("create") => f.create = true,
            Some("excl") => f.excl = true,
            Some("trunc") => f.trunc = true,
            Some("append") => f.append = true,
            Some("dsync") => f.dsync = true,
            Some("sync") => f.sync = true,
            Some("directory") => f.directory = true,
            other => return Err(format!("{at}.flags: unknown flag {other:?}")),
        }
    }
    Ok(f)
}

/// Run the first `n` ops of the workload in a fresh model kernel.
fn run_ops(ops: &[Value], n: usize, profile: Profile, swarm: &Swarm) -> Result<Kernel, String> {
    let mut k = Kernel::new(KernelConfig { profile, ..KernelConfig::default() });
    let choices = SeededChoices::new(0);
    let mut trace = Trace::new();
    let pid = 1;
    k.spawn(pid);
    let mut fds: BTreeMap<String, Fd> = BTreeMap::new();
    for (i, op) in ops.iter().take(n).enumerate() {
        let at = format!("ops[{i}]");
        let mut call = |c: Syscall| -> Result<SysOk, String> {
            let mut env = hegel_sim_core::kernel::Env { choices: &choices, swarm, trace: &mut trace, step: i as u64, t_ns: 0, who: "workload" };
            match k.syscall(pid, &c, &mut env) {
                SysResult::Ok(ok) => Ok(ok),
                r => Err(format!("{at}: the model answered {} with {r:?}, where the real run succeeded", c.name())),
            }
        };
        let fill = || -> Result<Vec<u8>, String> {
            let f = field(op, "fill", &at)?;
            Ok(vec![num(f, "byte", &at)? as u8; num(f, "len", &at)? as usize])
        };
        let fd = |name: &str| fds.get(name).copied().ok_or_else(|| format!("{at}: fd {name:?} is not open"));
        match text(op, "op", &at)? {
            "mkdir" => drop(call(Syscall::Mkdir { path: text(op, "path", &at)?.into() })?),
            "rmdir" => drop(call(Syscall::Rmdir { path: text(op, "path", &at)?.into() })?),
            "unlink" => drop(call(Syscall::Unlink { path: text(op, "path", &at)?.into() })?),
            "rename" => drop(call(Syscall::Rename { from: text(op, "from", &at)?.into(), to: text(op, "to", &at)?.into() })?),
            "fsync_dir" => {
                let flags = OpenFlags { read: true, directory: true, ..OpenFlags::default() };
                let SysOk::Fd(d) = call(Syscall::Open { path: text(op, "path", &at)?.into(), flags })? else { unreachable!() };
                call(Syscall::Fsync { fd: d })?;
                call(Syscall::Close { fd: d })?;
            }
            "open" => {
                let SysOk::Fd(d) = call(Syscall::Open { path: text(op, "path", &at)?.into(), flags: open_flags(op, &at)? })? else { unreachable!() };
                fds.insert(text(op, "fd", &at)?.into(), d);
            }
            "write" => drop(call(Syscall::Write { fd: fd(text(op, "fd", &at)?)?, data: fill()? })?),
            "pwrite" => drop(call(Syscall::Pwrite { fd: fd(text(op, "fd", &at)?)?, data: fill()?, offset: num(op, "offset", &at)? })?),
            "fsync" => drop(call(Syscall::Fsync { fd: fd(text(op, "fd", &at)?)? })?),
            "fdatasync" => drop(call(Syscall::Fdatasync { fd: fd(text(op, "fd", &at)?)? })?),
            "ftruncate" => drop(call(Syscall::Ftruncate { fd: fd(text(op, "fd", &at)?)?, len: num(op, "len", &at)? })?),
            "close" => {
                let name = text(op, "fd", &at)?;
                call(Syscall::Close { fd: fd(name)? })?;
                fds.remove(name);
            }
            other => return Err(format!("{at}: unknown op {other:?}")),
        }
    }
    Ok(k)
}

/// The op prefixes a crash point allows: a mark `op-i` is exactly after op
/// `i`; a flush or subset state labelled `op-i` is after op `i`, with op `i+1`
/// possibly in flight.
fn prefixes(cp: &Value, n_ops: usize) -> Result<Vec<usize>, String> {
    let kind = text(cp, "kind", "crash_point")?;
    let label = text(cp, "label", "crash_point")?;
    let done = match label {
        "mkfs" => 0,
        "unmounted" => n_ops,
        l => l.strip_prefix("op-").and_then(|i| i.parse::<usize>().ok()).map(|i| i + 1).ok_or_else(|| format!("crash_point.label: {l:?} is not mkfs, op-<i> or unmounted"))?,
    };
    Ok(match kind {
        "mark" => vec![done],
        "flush" | "subset" => {
            let mut v = vec![done];
            if done < n_ops {
                v.push(done + 1);
            }
            v
        }
        other => return Err(format!("crash_point.kind: unknown kind {other:?}")),
    })
}

fn check(workload: &Value, observed: &Value) -> Result<(bool, String, Vec<String>), String> {
    let ops = field(workload, "ops", "workload")?.as_array().ok_or("workload.ops: expected a list")?;
    let fs = text(observed, "fs", "observed")?;
    let (profile, shapes, model) = model_for(fs)?;
    let swarm = Swarm::fixed(shapes, Rates::default());
    let mut want: BTreeMap<String, Observed> = BTreeMap::new();
    for (p, e) in field(observed, "entries", "observed")?.as_object().ok_or("observed.entries: expected an object")? {
        if p == "/" {
            continue;
        }
        let at = format!("entries[{p:?}]");
        let o = match text(e, "type", &at)? {
            "dir" => Observed::Dir,
            "file" => Observed::File(b64(text(e, "content_b64", &at)?)?),
            other => return Ok((false, format!("{p} is a {other}; the workload makes only files and directories"), vec![model.into()])),
        };
        want.insert(p.clone(), o);
    }
    let mut why = Vec::new();
    for n in prefixes(field(observed, "crash_point", "observed")?, ops.len())? {
        let k = run_ops(ops, n, profile, &swarm)?;
        match k.could_crash_to(CrashKind::PowerLoss, &swarm, &want) {
            Ok(how) => return Ok((true, format!("power loss after {n} ops: {how}"), vec![model.into(), format!("after {n} ops"), how])),
            Err(e) => why.push(format!("after {n} ops: {e}")),
        }
    }
    Ok((false, why.join("; "), vec![model.into()]))
}

fn self_test(dir: &str, seeds: u64) -> Result<String, String> {
    let mut paths: Vec<_> = std::fs::read_dir(dir).map_err(|e| format!("{dir}: {e}"))?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    let mut checked = 0u64;
    for path in paths.iter().filter(|p| p.extension().is_some_and(|x| x == "json")) {
        let workload: Value = serde_json::from_slice(&std::fs::read(path).map_err(|e| e.to_string())?).map_err(|e| format!("{}: {e}", path.display()))?;
        let ops = field(&workload, "ops", "workload")?.as_array().ok_or("workload.ops: expected a list")?;
        for fs in ["ext4-ordered", "ext4-writeback", "xfs", "btrfs"] {
            let (profile, shapes, _) = model_for(fs)?;
            let swarm = Swarm::fixed(shapes.iter().chain([Fault::PowerLoss]).collect(), Rates::default());
            for n in 0..=ops.len() {
                for seed in 0..seeds {
                    let mut k = run_ops(ops, n, profile, &swarm)?;
                    let choices = SeededChoices::new(seed);
                    let mut trace = Trace::new();
                    let mut env = hegel_sim_core::kernel::Env { choices: &choices, swarm: &swarm, trace: &mut trace, step: 0, t_ns: 0, who: "crash" };
                    k.crash(CrashKind::PowerLoss, &mut env);
                    let mut entries = serde_json::Map::new();
                    entries.insert("/".into(), json!({"type": "dir"}));
                    for (p, dir) in k.walk() {
                        let e = if dir {
                            json!({"type": "dir"})
                        } else {
                            let data = k.read_file(&p).unwrap_or_default();
                            json!({"type": "file", "size": data.len(), "content_b64": b64_encode(&data)})
                        };
                        entries.insert(p, e);
                    }
                    let label = if n == 0 { "mkfs".to_owned() } else { format!("op-{}", n - 1) };
                    let observed = json!({"fs": fs, "crash_point": {"kind": "mark", "label": label, "entries": []}, "entries": entries});
                    let (allowed, reason, _) = check(&workload, &observed)?;
                    if !allowed {
                        return Err(format!("{} on {fs}, after {n} ops, seed {seed}: the model drew a state its checker refuses: {reason}", path.display()));
                    }
                    checked += 1;
                }
            }
        }
    }
    Ok(format!("{checked} model-drawn crash states, all allowed"))
}

fn b64_encode(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in data.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        for i in 0..4 {
            out.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--self-test") {
        let dir = args.get(2).map_or("sim/validation/workloads", String::as_str);
        let seeds = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(200);
        match self_test(dir, seeds) {
            Ok(s) => println!("{s}"),
            Err(e) => {
                eprintln!("crash_states --self-test: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    let verdict = (|| -> Result<(bool, String, Vec<String>), String> {
        let path = std::env::args().nth(1).ok_or("usage: crash_states <workload.json> < observed.json")?;
        let workload: Value = serde_json::from_slice(&std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?).map_err(|e| format!("{path}: {e}"))?;
        let mut input = String::new();
        std::io::stdin().read_to_string(&mut input).map_err(|e| format!("stdin: {e}"))?;
        let observed: Value = serde_json::from_str(&input).map_err(|e| format!("stdin: {e}"))?;
        check(&workload, &observed)
    })();
    match verdict {
        Ok((allowed, reason, rules)) => println!("{}", json!({"allowed": allowed, "reason": reason, "rules": rules})),
        Err(e) => {
            eprintln!("crash_states: {e}");
            std::process::exit(2);
        }
    }
}
