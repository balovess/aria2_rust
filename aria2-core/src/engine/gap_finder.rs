pub fn find_first_gap(completed_ranges: &[(u64, u64)], total_length: u64) -> u64 {
    if completed_ranges.is_empty() {
        return 0;
    }

    let mut expected_offset = 0;
    for &(offset, length) in completed_ranges {
        if offset > expected_offset {
            return expected_offset;
        }
        expected_offset = offset + length;
    }
    expected_offset.min(total_length)
}

pub fn find_all_gaps(completed_ranges: &[(u64, u64)], total_length: u64) -> Vec<(u64, u64)> {
    let mut gaps = Vec::new();
    if completed_ranges.is_empty() {
        if total_length > 0 {
            gaps.push((0, total_length));
        }
        return gaps;
    }

    let mut current = 0;
    for &(offset, length) in completed_ranges {
        if offset > current {
            gaps.push((current, offset - current));
        }
        current = std::cmp::max(current, offset + length);
    }
    if current < total_length {
        gaps.push((current, total_length - current));
    }
    gaps
}
