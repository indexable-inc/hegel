RELEASE_TYPE: patch

This patch adds external generation to the C ABI. A run started with `hegel_run_start_external` takes each generation case from the caller as data instead of drawing it at random: when it wants one, `hegel_next_test_case` returns the new `HEGEL_E_NEEDS_PROPOSAL`, and the caller answers with `hegel_run_propose` (a choice prefix in the example database's encoding, plus a `hegel_tail_t` saying whether to draw past it from a generator seeded by the proposal or at the simplest value of every draw) or ends generation with `hegel_run_end_generation`.

After running a proposed case, the run makes its report available to `hegel_run_case_report`: every draw with its kind, bounds and value, the spans, the status, and the targets and events the test recorded. Everything outside the generation phase is unchanged, so database replay, shrinking and saving failures work as in `hegel_run_start`. Runs started with `hegel_run_start` behave exactly as before.
