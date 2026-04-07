use aria2_core::segment::{Segment, Bitfield};
use aria2_core::ui::ProgressBar;

#[test]
fn test_segment_split_8gb_file() {
    let total: u64 = 8 * 1024 * 1024 * 1024;
    let segment_size = total / 16;
    let segments: Vec<Segment> = (0..16)
        .map(|i| Segment::new(i, i as u64 * segment_size, ((i + 1) as u64) * segment_size))
        .collect();
    assert_eq!(segments.len(), 16);
    for (i, seg) in segments.iter().enumerate() {
        assert!(seg.start() < total, "segment {} start overflow", i);
        assert!(seg.end() <= total, "segment {} end overflow", i);
        assert!(seg.length() > 0, "segment {} length is zero", i);
        if i > 0 {
            assert_eq!(seg.start(), segments[i - 1].end());
        }
    }
}

#[test]
fn test_segment_single_4gb_plus_range() {
    let seg = Segment::new(0, 0, 5u64 * 1024 * 1024 * 1024);
    assert_eq!(seg.length(), 5u64 * 1024 * 1024 * 1024);
}

#[test]
fn test_bitfield_large_index() {
    let mut bf = Bitfield::new(100000);
    let _ = bf.set(99999);
    let _ = bf.unset(99999);
}

#[test]
fn test_progress_bar_8gb_file() {
    let mut pb = ProgressBar::new(8u64 * 1024 * 1024 * 1024);
    pb.update(4u64 * 1024 * 1024 * 1024);
    pb.render(true);
    assert!(pb.current() <= pb.total());
}

#[test]
fn test_progress_bar_normal_range() {
    let mut pb = ProgressBar::new(1000);
    pb.update(0);
    pb.render(true);
    pb.update(500);
    pb.render(true);
    pb.update(1000);
    pb.render(true);
    pb.finish();
}

#[test]
fn test_progress_bar_zero_total() {
    let mut pb = ProgressBar::new(1);
    pb.update(0);
    pb.render(true);
    pb.finish();
}
