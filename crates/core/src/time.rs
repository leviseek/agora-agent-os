//! Time helpers. The runtime only ever stores epoch milliseconds.

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the unix epoch. Cheap to serialize, trivially comparable, timezone-free.
pub type Timestamp = u64;

pub fn now_ms() -> Timestamp {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64
}

pub fn elapsed_ms(since: Timestamp) -> u64 {
    now_ms().saturating_sub(since)
}

/// RFC3339 rendering for humans and for HTTP responses.
pub fn to_rfc3339(ms: Timestamp) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_monotonicish() {
        let a = now_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(now_ms() >= a);
    }

    #[test]
    fn rfc3339_is_rendered() {
        let s = to_rfc3339(0);
        assert!(s.starts_with("1970-01-01"), "got {s}");
    }
}
