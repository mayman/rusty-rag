//! The system clock: the only place "now" is read.

use std::time::{SystemTime, UNIX_EPOCH};

use rag_core::application::ports::Clock;
use rag_core::domain::indexed_at::IndexedAt;

/// `Clock` backed by the operating system.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> IndexedAt {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX)
            });
        IndexedAt::from_epoch_seconds(seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::SystemClock;
    use rag_core::application::ports::Clock;

    #[test]
    fn now_is_after_2024_and_does_not_go_backwards() {
        let first = SystemClock.now();
        let second = SystemClock.now();
        assert!(first.epoch_seconds() > 1_700_000_000);
        assert!(second.epoch_seconds() >= first.epoch_seconds());
    }
}
