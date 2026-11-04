//! Run a scenario under Hegel and report which case first failed.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicU64, Ordering};

use hegel::{HealthCheck, Hegel, Settings, TestCase};

/// What a Hegel run of a scenario did.
#[derive(Debug)]
pub struct Verdict {
    /// The 1-based index of the first failing case, before shrinking.
    pub first_failure: Option<u64>,
    /// The panic message Hegel ended with (the shrunk failure), if any.
    pub message: Option<String>,
}

fn message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_default()
}

/// Run `scenario` for up to `cases` generated cases from `seed`.
pub fn run(cases: u64, seed: u64, scenario: impl Fn(&TestCase)) -> Verdict {
    let count = AtomicU64::new(0);
    let first = AtomicU64::new(0);
    let settings = Settings::new()
        .test_cases(cases)
        .seed(Some(seed))
        .database(None)
        .suppress_health_check(HealthCheck::all());
    let result = catch_unwind(AssertUnwindSafe(|| {
        Hegel::new(|tc: TestCase| {
            let n = count.fetch_add(1, Ordering::SeqCst) + 1;
            if let Err(p) = catch_unwind(AssertUnwindSafe(|| scenario(&tc))) {
                let _ = first.compare_exchange(0, n, Ordering::SeqCst, Ordering::SeqCst);
                resume_unwind(p);
            }
        })
        .settings(settings)
        .run();
    }));
    let f = first.load(Ordering::SeqCst);
    Verdict { first_failure: (f != 0).then_some(f), message: result.err().map(|p| message(&*p)) }
}
