//! LineRange: a 1-based, inclusive span of lines within one document.

/// A span of lines, `start..=end`, both 1-based. Always `1 <= start <= end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineRange {
    start: usize,
    end: usize,
}

impl LineRange {
    /// Returns `None` unless `1 <= start <= end`.
    pub fn new(start: usize, end: usize) -> Option<Self> {
        (start >= 1 && start <= end).then_some(Self { start, end })
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn end(&self) -> usize {
        self.end
    }

    /// True when the 1-based `line` lies inside the range (both ends inclusive).
    pub fn contains(&self, line: usize) -> bool {
        (self.start..=self.end).contains(&line)
    }
}

#[cfg(test)]
mod tests {
    use super::LineRange;

    #[test]
    fn accepts_a_range_with_start_not_after_end() {
        let range = LineRange::new(3, 5).expect("3..=5 is a valid range");
        assert_eq!((range.start(), range.end()), (3, 5));
    }

    #[test]
    fn accepts_a_single_line_range() {
        assert!(LineRange::new(7, 7).is_some());
    }

    #[test]
    fn rejects_line_zero_because_lines_are_one_based() {
        assert_eq!(LineRange::new(0, 4), None);
    }

    #[test]
    fn rejects_a_reversed_range() {
        assert_eq!(LineRange::new(5, 4), None);
    }

    #[test]
    fn contains_is_inclusive_at_both_ends() {
        let range = LineRange::new(2, 4).expect("valid range");
        assert!(range.contains(2) && range.contains(4));
        assert!(!range.contains(1) && !range.contains(5));
    }
}
