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
