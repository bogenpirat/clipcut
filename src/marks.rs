//! In/out marks defining the clip to export.
//!
//! Pure logic, no UI. The interesting decisions are what happens when the marks
//! would cross: silently reordering them confuses ("I set in at 30s, why is it
//! at 20s?"), and silently refusing is worse still, because nothing visibly
//! happens. So placing one mark past the other **drops the other one** — the
//! selection you were building is gone, which is honest and obvious.

/// Shortest exportable clip. Below roughly one frame there is nothing to cut.
const MIN_CLIP_SECONDS: f64 = 0.01;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Marks {
    pub in_point: Option<f64>,
    pub out_point: Option<f64>,
}

impl Marks {
    /// Place the in-mark, dropping the out-mark if it would end up behind it.
    pub fn set_in(&mut self, at: f64, duration: f64) {
        let at = clamp(at, duration);
        self.in_point = Some(at);
        if self.out_point.is_some_and(|out| out <= at) {
            self.out_point = None;
        }
    }

    /// Place the out-mark, dropping the in-mark if it would end up ahead of it.
    pub fn set_out(&mut self, at: f64, duration: f64) {
        let at = clamp(at, duration);
        self.out_point = Some(at);
        if self.in_point.is_some_and(|start| start >= at) {
            self.in_point = None;
        }
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// `(start, duration)` when both marks are set and enclose a usable span.
    pub fn selection(&self) -> Option<(f64, f64)> {
        let (start, end) = (self.in_point?, self.out_point?);
        let length = end - start;
        (length >= MIN_CLIP_SECONDS).then_some((start, length))
    }

    pub fn is_exportable(&self) -> bool {
        self.selection().is_some()
    }

    /// What still needs doing, for the status line. `None` when ready.
    pub fn missing(&self) -> Option<&'static str> {
        match (self.in_point, self.out_point) {
            (None, None) => Some("Set an in and out point to export"),
            (Some(_), None) => Some("Set an out point"),
            (None, Some(_)) => Some("Set an in point"),
            (Some(_), Some(_)) if !self.is_exportable() => Some("The selection is too short"),
            _ => None,
        }
    }
}

fn clamp(value: f64, duration: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }
    let upper = if duration.is_finite() && duration > 0.0 {
        duration
    } else {
        f64::MAX
    };
    value.clamp(0.0, upper)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUR: f64 = 100.0;

    #[test]
    fn marks_start_empty() {
        let m = Marks::default();
        assert!(!m.is_exportable());
        assert_eq!(m.selection(), None);
        assert!(m.missing().is_some());
    }

    #[test]
    fn a_normal_selection_reports_start_and_length() {
        let mut m = Marks::default();
        m.set_in(10.0, DUR);
        m.set_out(25.5, DUR);
        assert_eq!(m.selection(), Some((10.0, 15.5)));
        assert!(m.is_exportable());
        assert_eq!(m.missing(), None);
    }

    #[test]
    fn marks_are_clamped_to_the_file() {
        let mut m = Marks::default();
        m.set_in(-5.0, DUR);
        m.set_out(500.0, DUR);
        assert_eq!(m.in_point, Some(0.0));
        assert_eq!(m.out_point, Some(DUR));
    }

    #[test]
    fn an_unknown_duration_does_not_clamp_to_zero() {
        // Duration is unavailable briefly after loading; marks must still work.
        let mut m = Marks::default();
        m.set_in(12.0, 0.0);
        assert_eq!(m.in_point, Some(12.0));
        m.set_out(30.0, f64::NAN);
        assert_eq!(m.out_point, Some(30.0));
    }

    #[test]
    fn moving_in_past_out_drops_out() {
        let mut m = Marks::default();
        m.set_in(10.0, DUR);
        m.set_out(20.0, DUR);
        m.set_in(30.0, DUR);
        assert_eq!(m.in_point, Some(30.0));
        assert_eq!(m.out_point, None, "the stale out-mark must not survive");
    }

    #[test]
    fn moving_out_before_in_drops_in() {
        let mut m = Marks::default();
        m.set_in(10.0, DUR);
        m.set_out(20.0, DUR);
        m.set_out(5.0, DUR);
        assert_eq!(m.out_point, Some(5.0));
        assert_eq!(m.in_point, None);
    }

    #[test]
    fn coincident_marks_are_treated_as_crossing() {
        let mut m = Marks::default();
        m.set_in(10.0, DUR);
        m.set_out(10.0, DUR);
        // A zero-length clip is meaningless, so this is not left as a selection.
        assert_eq!(m.in_point, None);
        assert!(!m.is_exportable());
    }

    #[test]
    fn a_selection_shorter_than_a_frame_is_not_exportable() {
        let mut m = Marks::default();
        m.set_in(10.0, DUR);
        m.set_out(10.001, DUR);
        assert!(!m.is_exportable());
        assert_eq!(m.missing(), Some("The selection is too short"));
    }

    #[test]
    fn missing_describes_what_is_needed() {
        let mut m = Marks::default();
        assert_eq!(m.missing(), Some("Set an in and out point to export"));
        m.set_in(1.0, DUR);
        assert_eq!(m.missing(), Some("Set an out point"));
        m.clear();
        m.set_out(9.0, DUR);
        assert_eq!(m.missing(), Some("Set an in point"));
    }

    #[test]
    fn clearing_resets_everything() {
        let mut m = Marks::default();
        m.set_in(1.0, DUR);
        m.set_out(9.0, DUR);
        m.clear();
        assert_eq!(m, Marks::default());
    }
}
