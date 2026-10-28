//! In-process exercise of external generation over the C ABI:
//! `hegel_run_start_external`, `hegel_run_propose`,
//! `hegel_run_end_generation` and `hegel_run_case_report`.

mod common;

use common::{last_error, make_settings_no_db, ok, start};
use hegel_c::hegel_result_t::*;
use hegel_c::{
    HegelContext, HegelRun, HegelSettings, HegelTestCase, hegel_bytes_t, hegel_context_free,
    hegel_context_new, hegel_generate_boolean, hegel_generate_integer, hegel_mark_complete,
    hegel_next_test_case, hegel_result_t, hegel_run_case_report, hegel_run_end_generation,
    hegel_run_free, hegel_run_propose, hegel_run_start_external, hegel_settings_free,
    hegel_settings_set_test_cases, hegel_status_t, hegel_tail_t, hegel_test_case_clone,
    hegel_test_case_free,
};
use std::ptr;

const RANDOM: u32 = hegel_tail_t::HEGEL_TAIL_RANDOM as u32;
const SIMPLEST: u32 = hegel_tail_t::HEGEL_TAIL_SIMPLEST as u32;

fn bytes(data: &[u8]) -> hegel_bytes_t {
    hegel_bytes_t {
        data: data.as_ptr(),
        len: data.len(),
    }
}

fn int_prefix(values: &[i64]) -> Vec<u8> {
    let mut out = (values.len() as u32).to_le_bytes().to_vec();
    for value in values {
        out.extend_from_slice(&[0, 10]);
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

unsafe fn start_external(ctx: *mut HegelContext, s: *const HegelSettings) -> *mut HegelRun {
    let mut run: *mut HegelRun = ptr::null_mut();
    ok(unsafe { hegel_run_start_external(ctx, s, None, ptr::null_mut(), &mut run) });
    assert!(!run.is_null());
    run
}

/// What `hegel_next_test_case` handed back: a case, a proposal request, or
/// the end of the run.
enum Step {
    Case(*mut HegelTestCase),
    NeedsProposal,
    Finished,
}

unsafe fn step(ctx: *mut HegelContext, run: *mut HegelRun) -> Step {
    let mut tc: *mut HegelTestCase = ptr::null_mut();
    match unsafe { hegel_next_test_case(ctx, run, &mut tc) } {
        HEGEL_E_NEEDS_PROPOSAL => {
            assert!(tc.is_null());
            Step::NeedsProposal
        }
        HEGEL_OK if tc.is_null() => Step::Finished,
        HEGEL_OK => Step::Case(tc),
        rc => panic!("hegel_next_test_case returned {rc:?}: {}", last_error(ctx)),
    }
}

unsafe fn report(ctx: *mut HegelContext, run: *mut HegelRun) -> Option<Vec<u8>> {
    let mut out = hegel_bytes_t {
        data: ptr::null(),
        len: 0,
    };
    match unsafe { hegel_run_case_report(ctx, run, &mut out) } {
        HEGEL_OK => Some(unsafe { std::slice::from_raw_parts(out.data, out.len) }.to_vec()),
        HEGEL_E_NOT_FOUND => {
            assert!(out.data.is_null() && out.len == 0);
            None
        }
        rc => panic!("hegel_run_case_report returned {rc:?}"),
    }
}

unsafe fn complete(ctx: *mut HegelContext, tc: *mut HegelTestCase, status: hegel_status_t) {
    ok(unsafe { hegel_mark_complete(ctx, tc, status as u32, ptr::null()) });
    ok(unsafe { hegel_test_case_free(ctx, tc) });
}

/// A report decoded just far enough to check it: its id, status, and each
/// top-level draw's tag, with a clone's nested tags.
#[derive(Debug, PartialEq)]
struct Summary {
    id: u64,
    status: u8,
    draws: Vec<(u8, Vec<u8>)>,
    first_int: Option<i64>,
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> &[u8] {
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        head
    }
    fn u8(&mut self) -> u8 {
        self.take(1)[0]
    }
    fn u32(&mut self) -> u32 {
        u32::from_le_bytes(self.take(4).try_into().unwrap())
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take(8).try_into().unwrap())
    }
    fn int(&mut self) -> i64 {
        let len = self.u32() as usize;
        let raw = self.take(len);
        let fill = if raw.last().is_some_and(|b| b & 0x80 != 0) {
            0xff
        } else {
            0
        };
        let mut wide = [fill; 8];
        wide[..raw.len()].copy_from_slice(raw);
        i64::from_le_bytes(wide)
    }
    fn opt(&mut self, read: impl FnOnce(&mut Self)) {
        if self.u8() == 1 {
            read(self);
        }
    }
    fn str(&mut self) {
        let len = self.u32() as usize;
        self.take(len);
    }
    fn stream(&mut self, ints: &mut Vec<i64>) -> Vec<(u8, Vec<u8>)> {
        let mut draws = Vec::new();
        for _ in 0..self.u32() {
            let tag = self.u8();
            self.u8();
            let mut nested = Vec::new();
            match tag {
                0 => {
                    for _ in 0..3 {
                        self.int();
                    }
                    ints.push(self.int());
                }
                1 => {
                    self.u64();
                    self.u8();
                }
                5 => {
                    nested = self
                        .stream(&mut Vec::new())
                        .into_iter()
                        .map(|d| d.0)
                        .collect()
                }
                _ => panic!("unexpected tag {tag}"),
            }
            draws.push((tag, nested));
        }
        for _ in 0..self.u32() {
            self.take(8 + 8 + 8 + 4);
            self.opt(|r| {
                r.u64();
            });
            self.u8();
        }
        draws
    }
}

fn summarize(report: &[u8]) -> Summary {
    let mut r = Reader(report);
    assert_eq!(r.u8(), 1);
    let id = r.u64();
    let status = r.u8();
    r.opt(Reader::str);
    let mut ints = Vec::new();
    let draws = r.stream(&mut ints);
    for _ in 0..r.u32() {
        r.str();
        r.u64();
    }
    for _ in 0..r.u32() {
        r.str();
        r.opt(|r| {
            r.u64();
        });
    }
    assert!(r.0.is_empty(), "trailing bytes in {report:?}");
    Summary {
        id,
        status,
        draws,
        first_int: ints.first().copied(),
    }
}

/// Drive an external run to completion, answering the first proposal
/// requests from `proposals` (then ending generation) and running each
/// case with `body`. Returns each report read, in order.
unsafe fn drive(
    ctx: *mut HegelContext,
    run: *mut HegelRun,
    proposals: &[(u64, Vec<u8>, u32, u64)],
    mut body: impl FnMut(*mut HegelTestCase) -> hegel_status_t,
) -> Vec<Vec<u8>> {
    let mut reports = Vec::new();
    let mut next = proposals.iter();
    loop {
        let step = unsafe { step(ctx, run) };
        if let Some(report) = unsafe { report(ctx, run) } {
            reports.push(report);
        }
        match step {
            Step::Case(tc) => {
                let status = body(tc);
                unsafe { complete(ctx, tc, status) };
            }
            Step::NeedsProposal => match next.next() {
                Some((id, prefix, tail, seed)) => {
                    ok(unsafe { hegel_run_propose(ctx, run, *id, bytes(prefix), *tail, *seed) })
                }
                None => ok(unsafe { hegel_run_end_generation(ctx, run) }),
            },
            Step::Finished => return reports,
        }
    }
}

/// Draw an integer, or `None` when the case overran its choices.
unsafe fn draw_int(
    ctx: *mut HegelContext,
    tc: *mut HegelTestCase,
    min: i64,
    max: i64,
) -> Option<i64> {
    let mut value = 0;
    match unsafe { hegel_generate_integer(ctx, tc, min, max, &mut value) } {
        HEGEL_OK => Some(value),
        HEGEL_E_STOP_TEST => None,
        rc => panic!("hegel_generate_integer returned {rc:?}"),
    }
}

#[test]
fn an_external_run_runs_each_proposal_and_reports_it() {
    let ctx = hegel_context_new();
    unsafe {
        let s = make_settings_no_db(ctx);
        let run = start_external(ctx, s);
        let mut seen = Vec::new();
        let reports = drive(
            ctx,
            run,
            &[
                (5, int_prefix(&[3]), SIMPLEST, 0),
                (6, int_prefix(&[9]), RANDOM, 1),
            ],
            |tc| {
                seen.push(draw_int(ctx, tc, 0, 10).unwrap());
                hegel_status_t::HEGEL_STATUS_VALID
            },
        );
        let summaries: Vec<Summary> = reports.iter().map(|r| summarize(r)).collect();
        assert_eq!(
            summaries,
            vec![
                Summary {
                    id: 5,
                    status: 0,
                    draws: vec![(0, vec![])],
                    first_int: Some(3),
                },
                Summary {
                    id: 6,
                    status: 0,
                    draws: vec![(0, vec![])],
                    first_int: Some(9),
                },
            ]
        );
        assert!(seen.contains(&3) && seen.contains(&9), "{seen:?}");
        assert_eq!(report(ctx, run), None);
        ok(hegel_run_free(ctx, run));
        ok(hegel_settings_free(ctx, s));
        ok(hegel_context_free(ctx));
    }
}

#[test]
fn a_clone_reports_its_draws_as_one_nested_node() {
    let ctx = hegel_context_new();
    unsafe {
        let s = make_settings_no_db(ctx);
        let run = start_external(ctx, s);
        let reports = drive(ctx, run, &[(1, int_prefix(&[]), SIMPLEST, 0)], |tc| {
            let mut clone: *mut HegelTestCase = ptr::null_mut();
            ok(hegel_test_case_clone(ctx, tc, &mut clone));
            let mut value = false;
            ok(hegel_generate_boolean(
                ctx, clone, 0.5, false, false, &mut value,
            ));
            ok(hegel_test_case_free(ctx, clone));
            hegel_status_t::HEGEL_STATUS_VALID
        });
        assert_eq!(summarize(&reports[0]).draws, vec![(5, vec![1])]);
        ok(hegel_run_free(ctx, run));
        ok(hegel_settings_free(ctx, s));
        ok(hegel_context_free(ctx));
    }
}

#[test]
fn a_failing_proposal_is_reported_interesting_and_shrunk() {
    let ctx = hegel_context_new();
    unsafe {
        let s = make_settings_no_db(ctx);
        let run = start_external(ctx, s);
        let mut failing = Vec::new();
        let reports = drive(ctx, run, &[(1, int_prefix(&[90]), SIMPLEST, 0)], |tc| {
            let Some(n) = draw_int(ctx, tc, 0, 100) else {
                return hegel_status_t::HEGEL_STATUS_OVERRUN;
            };
            if n >= 10 {
                failing.push(n);
                hegel_status_t::HEGEL_STATUS_INTERESTING
            } else {
                hegel_status_t::HEGEL_STATUS_VALID
            }
        });
        assert_eq!(reports.len(), 1);
        assert_eq!(summarize(&reports[0]).status, 3);
        assert_eq!(failing.first(), Some(&90));
        assert_eq!(failing.last(), Some(&10));
        ok(hegel_run_free(ctx, run));
        ok(hegel_settings_free(ctx, s));
        ok(hegel_context_free(ctx));
    }
}

#[test]
fn generation_ends_at_the_test_case_budget_without_asking() {
    let ctx = hegel_context_new();
    unsafe {
        let s = make_settings_no_db(ctx);
        ok(hegel_settings_set_test_cases(ctx, s, 3));
        let run = start_external(ctx, s);
        let proposals: Vec<_> = (0..10)
            .map(|id| (id, int_prefix(&[]), RANDOM, id))
            .collect();
        let reports = drive(ctx, run, &proposals, |tc| {
            draw_int(ctx, tc, 0, 1000).unwrap();
            hegel_status_t::HEGEL_STATUS_VALID
        });
        assert!(reports.len() < 10, "{}", reports.len());
        ok(hegel_run_free(ctx, run));
        ok(hegel_settings_free(ctx, s));
        ok(hegel_context_free(ctx));
    }
}

fn expect_err(ctx: *mut HegelContext, rc: hegel_result_t, code: hegel_result_t, needle: &str) {
    assert_eq!(rc, code);
    let msg = last_error(ctx);
    assert!(msg.contains(needle), "{msg}");
}

#[test]
fn proposal_calls_refuse_a_run_that_is_not_waiting() {
    let ctx = hegel_context_new();
    unsafe {
        let s = make_settings_no_db(ctx);
        let run = start_external(ctx, s);
        let prefix = int_prefix(&[]);
        expect_err(
            ctx,
            hegel_run_propose(ctx, run, 0, bytes(&prefix), SIMPLEST, 0),
            HEGEL_E_INVALID_ARG,
            "hegel_run_propose: the run is not waiting for a proposal",
        );
        expect_err(
            ctx,
            hegel_run_end_generation(ctx, run),
            HEGEL_E_INVALID_ARG,
            "hegel_run_end_generation: the run is not waiting for a proposal",
        );
        ok(hegel_run_free(ctx, run));
        ok(hegel_settings_free(ctx, s));
        ok(hegel_context_free(ctx));
    }
}

#[test]
fn a_waiting_run_refuses_bad_proposals_and_a_premature_next() {
    let ctx = hegel_context_new();
    unsafe {
        let s = make_settings_no_db(ctx);
        let run = start_external(ctx, s);
        loop {
            match step(ctx, run) {
                Step::Case(tc) => {
                    draw_int(ctx, tc, 0, 1000).unwrap();
                    complete(ctx, tc, hegel_status_t::HEGEL_STATUS_VALID);
                }
                Step::NeedsProposal => break,
                Step::Finished => panic!("the run finished without asking for a proposal"),
            }
        }
        let mut tc: *mut HegelTestCase = ptr::null_mut();
        expect_err(
            ctx,
            hegel_next_test_case(ctx, run, &mut tc),
            HEGEL_E_NOT_COMPLETE,
            "waiting for a proposal",
        );
        let prefix = int_prefix(&[]);
        expect_err(
            ctx,
            hegel_run_propose(ctx, run, 0, bytes(&prefix), 99, 0),
            HEGEL_E_INVALID_ARG,
            "unknown tail 99",
        );
        expect_err(
            ctx,
            hegel_run_propose(ctx, run, 0, bytes(&[1, 0, 0, 0, 9]), SIMPLEST, 0),
            HEGEL_E_INVALID_ARG,
            "not a valid encoded choice sequence",
        );
        let mut trailing = prefix.clone();
        trailing.push(0);
        expect_err(
            ctx,
            hegel_run_propose(ctx, run, 0, bytes(&trailing), SIMPLEST, 0),
            HEGEL_E_INVALID_ARG,
            "not a valid encoded choice sequence",
        );
        let null = hegel_bytes_t {
            data: ptr::null(),
            len: 4,
        };
        expect_err(
            ctx,
            hegel_run_propose(ctx, run, 0, null, SIMPLEST, 0),
            HEGEL_E_INVALID_ARG,
            "prefix is null",
        );
        let huge = hegel_bytes_t {
            data: prefix.as_ptr(),
            len: usize::MAX,
        };
        expect_err(
            ctx,
            hegel_run_propose(ctx, run, 0, huge, SIMPLEST, 0),
            HEGEL_E_INVALID_ARG,
            "prefix length exceeds isize::MAX",
        );
        let empty = hegel_bytes_t {
            data: ptr::null(),
            len: 0,
        };
        expect_err(
            ctx,
            hegel_run_propose(ctx, run, 0, empty, SIMPLEST, 0),
            HEGEL_E_INVALID_ARG,
            "not a valid encoded choice sequence",
        );
        ok(hegel_run_propose(ctx, run, 0, bytes(&prefix), SIMPLEST, 0));
        ok(hegel_run_free(ctx, run));
        ok(hegel_settings_free(ctx, s));
        ok(hegel_context_free(ctx));
    }
}

#[test]
fn proposal_calls_refuse_null_handles() {
    let ctx = hegel_context_new();
    unsafe {
        let prefix = int_prefix(&[]);
        expect_err(
            ctx,
            hegel_run_propose(ctx, ptr::null_mut(), 0, bytes(&prefix), SIMPLEST, 0),
            HEGEL_E_INVALID_HANDLE,
            "hegel_run_propose: run pointer is null",
        );
        expect_err(
            ctx,
            hegel_run_end_generation(ctx, ptr::null_mut()),
            HEGEL_E_INVALID_HANDLE,
            "hegel_run_end_generation: run pointer is null",
        );
        let mut out = hegel_bytes_t {
            data: ptr::null(),
            len: 0,
        };
        expect_err(
            ctx,
            hegel_run_case_report(ctx, ptr::null_mut(), &mut out),
            HEGEL_E_INVALID_HANDLE,
            "hegel_run_case_report: run pointer is null",
        );
        let s = make_settings_no_db(ctx);
        let run = start_external(ctx, s);
        expect_err(
            ctx,
            hegel_run_case_report(ctx, run, ptr::null_mut()),
            HEGEL_E_INVALID_ARG,
            "hegel_run_case_report: out parameter is null",
        );
        expect_err(
            ctx,
            hegel_run_start_external(ctx, s, None, ptr::null_mut(), ptr::null_mut()),
            HEGEL_E_INVALID_ARG,
            "hegel_run_start_external: out parameter is null",
        );
        let mut other: *mut HegelRun = ptr::null_mut();
        expect_err(
            ctx,
            hegel_run_start_external(ctx, ptr::null(), None, ptr::null_mut(), &mut other),
            HEGEL_E_INVALID_HANDLE,
            "hegel_run_start_external: settings pointer is null",
        );
        ok(hegel_run_free(ctx, run));
        ok(hegel_settings_free(ctx, s));
        ok(hegel_context_free(ctx));
    }
}

#[test]
fn a_default_run_never_asks_for_a_proposal_or_reports() {
    let ctx = hegel_context_new();
    unsafe {
        let s = make_settings_no_db(ctx);
        ok(hegel_settings_set_test_cases(ctx, s, 20));
        let run = start(ctx, s);
        let mut cases = 0;
        loop {
            let step = step(ctx, run);
            assert_eq!(report(ctx, run), None);
            match step {
                Step::Case(tc) => {
                    draw_int(ctx, tc, 0, 1000).unwrap();
                    complete(ctx, tc, hegel_status_t::HEGEL_STATUS_VALID);
                    cases += 1;
                }
                Step::NeedsProposal => panic!("a default run asked for a proposal"),
                Step::Finished => break,
            }
        }
        assert!(cases > 1);
        ok(hegel_run_free(ctx, run));
        ok(hegel_settings_free(ctx, s));
        ok(hegel_context_free(ctx));
    }
}
