//! The wall-clock budget of one command (`--deadline`). One value is shared by the
//! progress reporter, which tells the use cases between phases that time is up, and
//! the git client, which caps every subprocess at the time that is left.

use std::time::{Duration, Instant};

/// A point in time after which the command must stop, or no limit.
#[derive(Debug, Clone, Copy)]
pub struct Deadline {
    end: Option<Instant>,
}

impl Deadline {
    /// No limit.
    pub fn none() -> Self {
        Self { end: None }
    }

    /// `seconds` from now; 0 means no limit (the meaning of `--deadline 0`).
    pub fn after_seconds(seconds: u64) -> Self {
        if seconds == 0 {
            Self::none()
        } else {
            Self::after(Duration::from_secs(seconds))
        }
    }

    /// `limit` from now. A zero `limit` has passed already.
    pub fn after(limit: Duration) -> Self {
        Self {
            end: Instant::now().checked_add(limit),
        }
    }

    /// Time left; `None` when there is no limit, zero when it has passed.
    pub fn remaining(&self) -> Option<Duration> {
        self.end
            .map(|end| end.saturating_duration_since(Instant::now()))
    }

    /// Whether the limit has passed.
    pub fn passed(&self) -> bool {
        self.remaining().is_some_and(|left| left.is_zero())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::Deadline;

    #[test]
    fn no_limit_never_passes_and_has_no_remaining_time() {
        let deadline = Deadline::none();
        assert!(!deadline.passed());
        assert_eq!(deadline.remaining(), None);
    }

    #[test]
    fn zero_seconds_means_no_limit() {
        assert_eq!(Deadline::after_seconds(0).remaining(), None);
    }

    #[test]
    fn a_limit_counts_down_and_then_has_passed() {
        let deadline = Deadline::after_seconds(3600);
        assert!(!deadline.passed());
        let left = deadline.remaining().unwrap();
        assert!(left > Duration::from_secs(3500) && left <= Duration::from_secs(3600));
        assert!(Deadline::after(Duration::ZERO).passed());
        assert_eq!(
            Deadline::after(Duration::ZERO).remaining(),
            Some(Duration::ZERO)
        );
    }
}
