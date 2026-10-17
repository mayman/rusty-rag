//! Retries of flaky operations with exponential backoff.

use std::thread::sleep;
use std::time::Duration;

/// How often and how long to retry an operation.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_delay: Duration,
}

impl RetryPolicy {
    pub fn standard() -> Self {
        Self {
            max_attempts: 5,
            initial_delay: Duration::from_millis(100),
        }
    }
}

/// Calls `attempt` until it succeeds or the policy runs out, doubling the delay
/// after each failure. The doubling saturates instead of wrapping.
pub fn retry_with_backoff<F, T, E>(policy: RetryPolicy, mut attempt: F) -> Result<T, E>
where
    F: FnMut() -> Result<T, E>,
{
    let mut delay = policy.initial_delay;
    let mut tries = 1;
    loop {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) if tries >= policy.max_attempts => return Err(error),
            Err(_) => {
                sleep(delay);
                delay = delay.saturating_mul(2);
                tries += 1;
            }
        }
    }
}
