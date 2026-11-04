//! The run's explanation: what a failing case prints.

use std::fmt::Write;

/// The most events kept; later ones are counted, not stored.
const MAX_EVENTS: usize = 4000;

/// One line of the trace.
#[derive(Clone, Debug)]
pub struct Event {
    /// Global scheduler step when it happened.
    pub step: u64,
    /// Virtual monotonic time, nanoseconds.
    pub t_ns: u64,
    /// What happened.
    pub text: String,
}

/// Swarm classes, spawns and exits, preemptions, faults that fired, crashes and
/// their resolution, and signals. Steps that were not preempted are not listed.
#[derive(Clone, Debug, Default)]
pub struct Trace {
    events: Vec<Event>,
    dropped: u64,
}

impl Trace {
    /// An empty trace.
    pub fn new() -> Self {
        Trace::default()
    }

    /// Record `text` at `step` and `t_ns`.
    pub fn push(&mut self, step: u64, t_ns: u64, text: impl Into<String>) {
        if self.events.len() >= MAX_EVENTS {
            self.dropped += 1;
            return;
        }
        self.events.push(Event {
            step,
            t_ns,
            text: text.into(),
        });
    }

    /// The recorded events.
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// The trace as text, one event per line.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for e in &self.events {
            let _ = writeln!(
                out,
                "  [step {:>6} t={:>10}] {}",
                e.step,
                fmt_ns(e.t_ns),
                e.text
            );
        }
        if self.dropped > 0 {
            let _ = writeln!(out, "  ... {} more events not kept", self.dropped);
        }
        out
    }
}

/// `ns` as a short human duration.
pub fn fmt_ns(ns: u64) -> String {
    if ns >= 1_000_000_000 {
        format!("{:.3}s", ns as f64 / 1e9)
    } else if ns >= 1_000_000 {
        format!("{:.3}ms", ns as f64 / 1e6)
    } else if ns >= 1_000 {
        format!("{:.1}us", ns as f64 / 1e3)
    } else {
        format!("{ns}ns")
    }
}
