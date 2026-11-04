RELEASE_TYPE: patch

This patch adds the `hegel::explore` module, which lets something outside Hegel choose a test's generation cases, such as a coverage-guided fuzzer. Implement `explore::Source` and pass it to `Hegel::run_with_source`: the run asks it for each case as a `Proposal` (a choice prefix plus how to draw past it) and hands it a `Report` of what each case drew, with every draw's kind and bounds, its spans and its status. Failures are still shrunk and saved to the database as usual.

```rust
use hegel::explore::{Next, Proposal, Report, Source, Tail};

struct Seeds(u64);

impl Source for Seeds {
    fn next(&mut self) -> Next {
        self.0 += 1;
        if self.0 > 100 {
            return Next::Done;
        }
        Next::Run(Proposal { id: self.0, prefix: vec![], tail: Tail::Random { seed: self.0 } })
    }

    fn observe(&mut self, report: &Report) {
        println!("{:?}", report.choices());
    }
}

hegel::Hegel::new(|tc| { /* ... */ }).run_with_source(&mut Seeds(0));
```

With the `static-engine` feature, `hegel` now builds for WASI (`wasm32-wasip1`, `wasm32-wasip2`), since its engine picks the WebAssembly platform backend there.

It also adds `hegel::sim` behind the new `sim` feature: deterministic simulation testing. `hegel::sim` is a facade over `std::sync`, `std::thread`, `std::time` and `std::fs` (plus `mmap` and signals) that is std itself until the feature is on. With the feature on, a `hegel::sim::Sim` runs programs written against the facade on a PCT scheduler, with a switch point at every atomic, lock and syscall, and on a simulated Linux or macOS kernel: page cache, fsync and directory fsync, power loss that tears what was not synced, ENOSPC, EIO and flock/fcntl locks. Processes can be killed, crashed and restarted, with a different version if you like. Every schedule and fault decision is a Hegel draw, so a failing run shrinks to a minimal schedule and replays, and the trace is printed with it.

```rust
use hegel::sim::{Config, Sim, fs};

#[hegel::test]
fn survives_power_loss(tc: hegel::TestCase) {
    let mut sim = Sim::new(&tc, Config::default());
    sim.spawn("writer", || { fs::write("/data", b"hello").unwrap(); 0 });
    sim.run();
    // check what recovery sees with sim.read_file(..)
}
```
