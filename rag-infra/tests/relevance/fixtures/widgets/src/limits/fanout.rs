//! Caps on how many downstream calls one request may fan out to.

/// Upper bound on concurrent downstream calls for a single request.
pub const QUOKKA_FANOUT_LIMIT: usize = 7;

/// How many more calls may start, given how many are already in flight.
pub fn remaining_fanout(in_flight: usize) -> usize {
    QUOKKA_FANOUT_LIMIT.saturating_sub(in_flight)
}

#[cfg(test)]
mod tests {
    use super::remaining_fanout;

    #[test]
    fn a_full_fanout_leaves_no_room() {
        assert_eq!(remaining_fanout(7), 0);
    }
}
