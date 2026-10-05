/// Lower a server's Range size limit after the current limit was rejected.
///
/// A stale rejection from an already larger in-flight request must not lower
/// the limit again: that rejection was caused by a limit that has already
/// been removed from future scheduling.
pub(super) fn lower_rejected_range_size_limit(
    current_limit: u64,
    rejected_limit: u64,
    configured_floor: u64,
) -> Option<u64> {
    if current_limit != rejected_limit {
        return None;
    }

    let floor = configured_floor
        .clamp(
            crate::constants::HTTP_RANGE_SIZE_FLOOR_MIN_BYTES,
            crate::constants::HTTP_RANGE_SIZE_FLOOR_MAX_BYTES,
        )
        .min(current_limit);
    if current_limit <= floor {
        return None;
    }

    Some((current_limit / 2).max(floor))
}

#[cfg(test)]
mod tests {
    use super::lower_rejected_range_size_limit;
    use crate::constants::DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES;

    #[test]
    fn halves_only_the_current_range_limit_and_clamps_to_the_floor() {
        assert_eq!(
            lower_rejected_range_size_limit(
                32 * 1024 * 1024,
                32 * 1024 * 1024,
                DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES,
            ),
            Some(16 * 1024 * 1024)
        );
        assert_eq!(
            lower_rejected_range_size_limit(
                96 * 1024,
                96 * 1024,
                DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES,
            ),
            Some(DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES)
        );
        assert_eq!(
            lower_rejected_range_size_limit(
                DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES,
                DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES,
                DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES,
            ),
            None
        );
        assert_eq!(
            lower_rejected_range_size_limit(
                16 * 1024 * 1024,
                32 * 1024 * 1024,
                DEFAULT_HTTP_RANGE_SIZE_FLOOR_BYTES,
            ),
            None,
            "an in-flight rejection from a superseded larger limit must not downshift twice"
        );
        assert_eq!(
            lower_rejected_range_size_limit(96 * 1024, 96 * 1024, 32 * 1024),
            Some(48 * 1024)
        );
        assert_eq!(
            lower_rejected_range_size_limit(96 * 1024, 96 * 1024, 80 * 1024),
            Some(80 * 1024)
        );
        assert_eq!(
            lower_rejected_range_size_limit(32 * 1024, 32 * 1024, 64 * 1024),
            None,
            "a configured floor larger than the initial piece must not expand it"
        );
    }
}
