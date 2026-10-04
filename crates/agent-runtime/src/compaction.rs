//! Compaction: turning the turns that fell out of the history window into one summary.
//!
//! The history window keeps the conversation faithful but bounded, which means old turns are
//! dropped. Compaction means they are dropped from the *prompt* rather than from *memory*: the
//! range that no longer fits is summarised once, stored as a memory record, and recalled later
//! like any other memory. A watermark in the actor state records how far compaction has come, so
//! a restart or a migration never re-summarises (or forgets) the same turns twice.

/// The transcript range to summarise now, as (start, end) with end exclusive.
///
/// The newest `keep` messages are never summarised: they are the part of the conversation the
/// user is still working with, and a summary of them would be strictly worse than the turns
/// themselves. Everything before the watermark is already covered by an earlier summary.
pub fn compaction_window(
    transcript_len: usize,
    compacted_through: usize,
    keep: usize,
    min_messages: usize,
) -> Option<(usize, usize)> {
    let end = transcript_len.saturating_sub(keep);
    let start = compacted_through.min(end);
    if end <= start {
        return None;
    }
    if end - start < min_messages.max(2) {
        // Summarising one exchange costs a model call and buys nothing an ordinary history turn
        // would not already carry.
        return None;
    }
    Some((start, end))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_to_do_inside_the_window() {
        assert_eq!(compaction_window(4, 0, 20, 4), None, "everything fits");
        assert_eq!(compaction_window(20, 0, 20, 4), None, "exactly at the edge");
    }

    #[test]
    fn compacts_only_what_the_window_dropped() {
        // 30 messages, keep 20 -> summarise 0..10
        assert_eq!(compaction_window(30, 0, 20, 4), Some((0, 10)));
        // already summarised through 6 -> continue from there
        assert_eq!(compaction_window(30, 6, 20, 4), Some((6, 10)));
    }

    #[test]
    fn a_short_remainder_does_not_earn_a_model_call() {
        assert_eq!(compaction_window(23, 0, 20, 4), None, "three dropped turns are not worth it");
        assert_eq!(compaction_window(24, 0, 20, 4), Some((0, 4)), "four are");
    }

    #[test]
    fn the_watermark_never_moves_backwards() {
        // A transcript that shrank (restore, replay) must not produce a negative or reversed range.
        assert_eq!(compaction_window(5, 40, 20, 4), None);
        assert_eq!(compaction_window(30, 40, 20, 4), None);
    }
}