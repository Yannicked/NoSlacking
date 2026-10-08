//! How long to wait before trying again.

use std::time::Duration;

/// `first` doubled `doublings` times, but never more than `longest`: the
/// wait before a retry, so a service that is down is asked less and less
/// often. Saturates rather than overflowing, however many doublings.
pub fn backoff(first: Duration, longest: Duration, doublings: u32) -> Duration {
    let factor = 1u32.checked_shl(doublings).unwrap_or(u32::MAX);
    first.saturating_mul(factor).min(longest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_longest() {
        let second = Duration::from_secs(1);
        let minute = Duration::from_secs(60);
        assert_eq!(backoff(second, minute, 0), second);
        assert_eq!(backoff(second, minute, 1), Duration::from_secs(2));
        assert_eq!(backoff(second, minute, 5), Duration::from_secs(32));
        assert_eq!(backoff(second, minute, 6), minute);
        assert_eq!(backoff(second, minute, 31), minute);
        assert_eq!(backoff(second, minute, u32::MAX), minute);
        assert_eq!(backoff(Duration::MAX, Duration::MAX, 3), Duration::MAX);
    }
}
