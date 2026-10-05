const MIN_PIECE_BYTES: u64 = 1024 * 1024;
const MAX_PIECE_BYTES: u64 = 32 * 1024 * 1024;
const TARGET_PIECE_COUNT: u64 = 64;

/// Select one immutable, power-of-two piece size for a known file length.
///
/// The target is approximately 64 pieces, bounded to 1–32 MiB. The count can
/// fall below 32 for files smaller than 32 MiB and exceed 256 for files larger
/// than 8 GiB because of those explicit size bounds.
pub(super) fn calculate_fixed_piece_size(file_size: u64) -> u64 {
    let raw_size = file_size / TARGET_PIECE_COUNT;
    raw_size
        .checked_next_power_of_two()
        .unwrap_or(MAX_PIECE_BYTES)
        .clamp(MIN_PIECE_BYTES, MAX_PIECE_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_power_of_two_tiers_for_file_length() {
        assert_eq!(
            calculate_fixed_piece_size(50 * MIN_PIECE_BYTES),
            MIN_PIECE_BYTES
        );
        assert_eq!(
            calculate_fixed_piece_size(1024 * MIN_PIECE_BYTES),
            16 * MIN_PIECE_BYTES
        );
        let eight_gib = 8 * 1024 * MIN_PIECE_BYTES;
        assert_eq!(calculate_fixed_piece_size(eight_gib), MAX_PIECE_BYTES);
        assert_eq!(eight_gib.div_ceil(MAX_PIECE_BYTES), 256);
        assert_eq!(calculate_fixed_piece_size(eight_gib + 1), MAX_PIECE_BYTES);
        assert_eq!((eight_gib + 1).div_ceil(MAX_PIECE_BYTES), 257);
    }

    #[test]
    fn clamps_tiny_and_extremely_large_files() {
        assert_eq!(calculate_fixed_piece_size(1), MIN_PIECE_BYTES);
        assert_eq!(calculate_fixed_piece_size(u64::MAX), MAX_PIECE_BYTES);
    }
}
