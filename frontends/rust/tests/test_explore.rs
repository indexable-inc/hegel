use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use hegel::explore::{Choice, Draw, Integer, Next, Proposal, Report, Source, Status, Tail};
use hegel::generators as gs;
use hegel::{Hegel, Settings, TestCase};

#[derive(Default)]
struct Script {
    proposals: VecDeque<Proposal>,
    reports: Vec<Report>,
    asked: usize,
}

impl Script {
    fn new(proposals: impl IntoIterator<Item = Proposal>) -> Self {
        Script {
            proposals: proposals.into_iter().collect(),
            ..Script::default()
        }
    }
}

impl Source for Script {
    fn next(&mut self) -> Next {
        self.asked += 1;
        match self.proposals.pop_front() {
            Some(proposal) => Next::Run(proposal),
            None => Next::Done,
        }
    }

    fn observe(&mut self, report: &Report) {
        self.reports.push(report.clone());
    }
}

fn int(value: i64) -> Choice {
    Choice::Integer(value.into())
}

fn proposal(id: u64, prefix: Vec<Choice>, tail: Tail) -> Proposal {
    Proposal { id, prefix, tail }
}

fn settings() -> Settings {
    Settings::new().database(None).test_cases(1000)
}

fn three_draws(tc: TestCase) {
    let _: i64 = tc.draw(gs::integers::<i64>().min_value(-5).max_value(10));
    let _: bool = tc.draw(gs::booleans());
    let _: Vec<u8> = tc.draw(gs::binary().max_size(4));
}

fn run(source: &mut Script, test: impl FnMut(TestCase)) {
    Hegel::new(test)
        .settings(settings())
        .run_with_source(source);
}

#[test]
fn the_prefix_is_honored_exactly() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let prefix = vec![int(7), Choice::Boolean(true), Choice::Bytes(vec![1, 2, 3])];
    let mut source = Script::new([proposal(1, prefix.clone(), Tail::Simplest)]);
    let record = Arc::clone(&seen);
    run(&mut source, move |tc| {
        let a: i64 = tc.draw(gs::integers::<i64>().min_value(-5).max_value(10));
        let b: bool = tc.draw(gs::booleans());
        let c: Vec<u8> = tc.draw(gs::binary().max_size(4));
        record.lock().unwrap().push((a, b, c));
    });
    assert_eq!(source.reports.len(), 1);
    assert_eq!(source.reports[0].choices(), prefix);
    assert!(seen.lock().unwrap().contains(&(7, true, vec![1, 2, 3])));
}

#[test]
fn a_choice_that_does_not_fit_its_draw_is_replaced() {
    let mut source = Script::new([proposal(
        1,
        vec![int(500), int(1), Choice::Bytes(vec![9])],
        Tail::Simplest,
    )]);
    run(&mut source, three_draws);
    let choices = source.reports[0].choices();
    let Choice::Integer(first) = choices[0] else {
        panic!("expected an integer, got {choices:?}");
    };
    assert!((-5..=10).contains(&first.to_i128().unwrap()), "{first}");
    assert!(matches!(choices[1], Choice::Boolean(_)), "{choices:?}");
    assert_eq!(choices[2], Choice::Bytes(vec![9]));
}

#[test]
fn reports_carry_kinds_bounds_spans_and_ids() {
    let mut source = Script::new([proposal(41, vec![int(3)], Tail::Simplest)]);
    run(&mut source, three_draws);
    let report = &source.reports[0];
    assert_eq!(report.id, 41);
    assert_eq!(report.status, Status::Valid);
    assert_eq!(report.origin, None);
    let Draw::Integer {
        min,
        max,
        shrink_towards,
        value,
    } = &report.nodes[0].draw
    else {
        panic!("expected an integer draw, got {:?}", report.nodes[0]);
    };
    assert_eq!(*shrink_towards, Integer::from(0));
    assert_eq!(
        (*min, *max, *value),
        (Integer::from(-5), Integer::from(10), Integer::from(3))
    );
    assert!(matches!(
        report.nodes[1].draw,
        Draw::Boolean { value: false, .. }
    ));
    assert!(matches!(
        &report.nodes[2].draw,
        Draw::Bytes { max_size: 4, value, .. } if value.is_empty()
    ));
    assert!(!report.spans.is_empty());
    for span in &report.spans {
        assert!(
            span.start <= span.end && span.end <= report.nodes.len(),
            "{span:?}"
        );
    }
}

#[test]
fn reports_carry_targets_and_events() {
    let mut source = Script::new([proposal(1, vec![int(4)], Tail::Simplest)]);
    run(&mut source, |tc| {
        let n: i64 = tc.draw(gs::integers::<i64>().min_value(0).max_value(10));
        tc.target_labelled(n as f64, "n");
        tc.event("drew");
    });
    let report = &source.reports[0];
    assert_eq!(report.targets, vec![("n".to_string(), 4.0)]);
    assert_eq!(report.events.len(), 1);
    assert!(report.events[0].0.contains("drew"), "{:?}", report.events);
}

#[test]
fn a_random_tail_is_a_function_of_its_seed() {
    let proposals = |seed| (0..3).map(move |id| proposal(id, vec![int(1)], Tail::Random { seed }));
    let draw_many = |tc: TestCase| {
        let _: Vec<i64> = tc.draw(gs::vecs(gs::integers::<i64>()).min_size(20).max_size(20));
    };
    let mut first = Script::new(proposals(5));
    run(&mut first, draw_many);
    let mut again = Script::new(proposals(5));
    run(&mut again, draw_many);
    let mut other = Script::new(proposals(6));
    run(&mut other, draw_many);
    assert_eq!(first.reports.len(), 3);
    assert_eq!(first.reports[0].choices(), first.reports[1].choices());
    assert_eq!(first.reports, again.reports);
    assert_ne!(first.reports[0].choices(), other.reports[0].choices());
    assert_eq!(first.reports[0].choices()[0], int(1));
}

#[test]
fn a_simplest_tail_draws_every_value_at_its_simplest() {
    let mut source = Script::new([proposal(1, vec![], Tail::Simplest)]);
    run(&mut source, |tc| {
        let _: i64 = tc.draw(gs::integers::<i64>().min_value(3).max_value(9));
        let _: bool = tc.draw(gs::booleans());
        let _: String = tc.draw(gs::text().min_size(2).max_size(5));
    });
    let report = &source.reports[0];
    assert!(matches!(
        report.nodes[0].draw,
        Draw::Integer { value, shrink_towards, .. }
            if value == Integer::from(3) && shrink_towards == Integer::from(3)
    ));
    assert!(matches!(
        report.nodes[1].draw,
        Draw::Boolean { value: false, .. }
    ));
    let strings: Vec<&Draw> = report
        .nodes
        .iter()
        .map(|n| &n.draw)
        .filter(|d| matches!(d, Draw::String { .. }))
        .collect();
    assert!(
        strings
            .iter()
            .all(|d| matches!(d, Draw::String { min_size, value, .. } if value.len() == *min_size)),
        "{strings:?}"
    );
}

#[test]
fn the_same_source_sequence_gives_the_same_reports() {
    let script = || {
        Script::new((0..20).map(|id| {
            let tail = if id % 2 == 0 {
                Tail::Random { seed: id }
            } else {
                Tail::Simplest
            };
            proposal(id, vec![int(id as i64 % 10)], tail)
        }))
    };
    let test = |tc: TestCase| {
        let n: i64 = tc.draw(gs::integers::<i64>().min_value(0).max_value(9));
        let _: Vec<bool> = tc.draw(gs::vecs(gs::booleans()).max_size(n as usize));
    };
    let mut a = script();
    run(&mut a, test);
    let mut b = script();
    run(&mut b, test);
    assert_eq!(a.reports.len(), 20);
    assert_eq!(a.reports, b.reports);
    assert_eq!(
        a.reports.iter().map(|r| r.id).collect::<Vec<_>>(),
        (0..20).collect::<Vec<_>>()
    );
}

#[test]
fn generation_ends_when_the_source_is_done() {
    let mut source = Script::new([
        proposal(1, vec![], Tail::Simplest),
        proposal(2, vec![], Tail::Simplest),
    ]);
    run(&mut source, three_draws);
    assert_eq!(source.asked, 3);
    assert_eq!(source.reports.len(), 2);
}

#[test]
fn generation_stops_at_the_test_case_budget() {
    let mut source = Script::new((0..50).map(|id| proposal(id, vec![], Tail::Random { seed: id })));
    Hegel::new(three_draws)
        .settings(Settings::new().database(None).test_cases(10))
        .run_with_source(&mut source);
    assert!(source.reports.len() < 50, "{}", source.reports.len());
}

fn failing(tc: TestCase) {
    let n: i64 = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000_000));
    assert!(n < 1000, "n = {n}");
}

#[test]
fn a_failing_external_case_is_shrunk_and_saved() {
    let db = tempfile::tempdir().unwrap();
    let db_path = db.path().to_str().unwrap().to_string();
    let last_failure = Arc::new(Mutex::new(None));
    let make_test = |last: Arc<Mutex<Option<i64>>>| {
        move |tc: TestCase| {
            let n: i64 = tc.draw(gs::integers::<i64>().min_value(0).max_value(1_000_000));
            if n >= 1000 {
                *last.lock().unwrap() = Some(n);
                panic!("n = {n}");
            }
        }
    };

    let mut source = Script::new([proposal(1, vec![int(777_777)], Tail::Simplest)]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Hegel::new(make_test(Arc::clone(&last_failure)))
            .settings(Settings::new().database(Some(db_path.clone())))
            .__database_key("explore-shrink".to_string())
            .run_with_source(&mut source);
    }));
    assert!(result.is_err());
    assert_eq!(source.reports[0].status, Status::Interesting);
    assert!(source.reports[0].origin.is_some());
    assert_eq!(*last_failure.lock().unwrap(), Some(1000));

    *last_failure.lock().unwrap() = None;
    let mut nothing = Script::new([]);
    let replay = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Hegel::new(make_test(Arc::clone(&last_failure)))
            .settings(Settings::new().database(Some(db_path)))
            .__database_key("explore-shrink".to_string())
            .run_with_source(&mut nothing);
    }));
    assert!(replay.is_err());
    assert_eq!(*last_failure.lock().unwrap(), Some(1000));
}

#[test]
fn a_reproduce_failure_blob_takes_precedence_over_the_source() {
    let mut source = Script::new([proposal(1, vec![], Tail::Simplest)]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Hegel::new(failing)
            .settings(settings())
            .reproduce_failure("not a blob")
            .run_with_source(&mut source);
    }));
    assert!(result.is_err());
    assert_eq!(source.asked, 0);
}

#[test]
fn clone_and_float_and_string_choices_round_trip_through_a_prefix() {
    let prefix = vec![
        Choice::Float(2.5),
        Choice::String(vec!['h' as u32, 'i' as u32]),
    ];
    let mut source = Script::new([proposal(1, prefix.clone(), Tail::Simplest)]);
    run(&mut source, |tc| {
        let _: f64 = tc.draw(gs::floats::<f64>().min_value(0.0).max_value(10.0));
        let _: String = tc.draw(gs::text().max_size(3));
    });
    let report = &source.reports[0];
    assert!(
        matches!(report.nodes[0].draw, Draw::Float { value, max, .. } if value == 2.5 && max == 10.0)
    );
    assert!(
        report.choices().contains(&prefix[1]),
        "{:?}",
        report.choices()
    );
}
