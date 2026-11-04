use crate::TestCase;
use crate::generators as gs;

impl hegel_sim::Choices for TestCase {
    fn int(&self, _label: &'static str, lo: u64, hi: u64) -> u64 {
        self.draw_silent(gs::integers::<u64>().min_value(lo).max_value(hi))
    }

    fn coin(&self, _label: &'static str, p: f64) -> bool {
        self.draw_silent(gs::weighted_booleans(p))
    }

    fn note(&self, line: &str) {
        TestCase::note(self, line);
    }
}
