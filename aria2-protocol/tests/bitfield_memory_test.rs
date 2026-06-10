//! Memory usage tests for Bitfield vs Vec<bool>
//!
//! This test verifies that Bitfield achieves the expected 8x memory reduction.

use aria2_protocol::bittorrent::piece::bitfield::Bitfield;

#[test]
fn test_memory_reduction_small() {
    let size = 100;
    let bf = Bitfield::new(size);
    
    let bf_memory = bf.memory_usage();
    let vec_memory = bf.vec_bool_memory_usage();
    let ratio = bf.memory_savings_ratio();
    
    println!("Small size ({}):", size);
    println!("  Bitfield: {} bytes", bf_memory);
    println!("  Vec<bool>: {} bytes", vec_memory);
    println!("  Ratio: {:.2}x", ratio);
    
    // Verify memory reduction
    assert!(bf_memory < vec_memory, "Bitfield should use less memory");
    assert!(ratio > 7.0, "Should achieve at least 7x memory savings");
}

#[test]
fn test_memory_reduction_medium() {
    let size = 10_000;
    let bf = Bitfield::new(size);
    
    let bf_memory = bf.memory_usage();
    let vec_memory = bf.vec_bool_memory_usage();
    let ratio = bf.memory_savings_ratio();
    
    println!("Medium size ({}):", size);
    println!("  Bitfield: {} bytes", bf_memory);
    println!("  Vec<bool>: {} bytes", vec_memory);
    println!("  Ratio: {:.2}x", ratio);
    
    // Verify memory reduction
    assert!(bf_memory < vec_memory, "Bitfield should use less memory");
    assert!(ratio > 7.9, "Should achieve close to 8x memory savings");
    
    // For 10,000 bits, Bitfield should use exactly 1,250 bytes
    assert_eq!(bf_memory, 1250, "10,000 bits should use 1,250 bytes");
    assert_eq!(vec_memory, 10_000, "Vec<bool> should use 10,000 bytes");
}

#[test]
fn test_memory_reduction_large() {
    let size = 100_000;
    let bf = Bitfield::new(size);
    
    let bf_memory = bf.memory_usage();
    let vec_memory = bf.vec_bool_memory_usage();
    let ratio = bf.memory_savings_ratio();
    
    println!("Large size ({}):", size);
    println!("  Bitfield: {} bytes", bf_memory);
    println!("  Vec<bool>: {} bytes", vec_memory);
    println!("  Ratio: {:.2}x", ratio);
    
    // Verify memory reduction
    assert!(bf_memory < vec_memory, "Bitfield should use less memory");
    assert!(ratio > 7.9, "Should achieve close to 8x memory savings");
    
    // For 100,000 bits, Bitfield should use exactly 12,500 bytes
    assert_eq!(bf_memory, 12_500, "100,000 bits should use 12,500 bytes");
    assert_eq!(vec_memory, 100_000, "Vec<bool> should use 100,000 bytes");
}

#[test]
fn test_memory_reduction_very_large() {
    let size = 1_000_000;
    let bf = Bitfield::new(size);
    
    let bf_memory = bf.memory_usage();
    let vec_memory = bf.vec_bool_memory_usage();
    let ratio = bf.memory_savings_ratio();
    
    println!("Very large size ({}):", size);
    println!("  Bitfield: {} bytes ({:.2} KB)", bf_memory, bf_memory as f64 / 1024.0);
    println!("  Vec<bool>: {} bytes ({:.2} KB)", vec_memory, vec_memory as f64 / 1024.0);
    println!("  Ratio: {:.2}x", ratio);
    
    // Verify memory reduction
    assert!(bf_memory < vec_memory, "Bitfield should use less memory");
    assert!(ratio > 7.9, "Should achieve close to 8x memory savings");
    
    // For 1,000,000 bits, Bitfield should use exactly 125,000 bytes
    assert_eq!(bf_memory, 125_000, "1,000,000 bits should use 125,000 bytes");
    assert_eq!(vec_memory, 1_000_000, "Vec<bool> should use 1,000,000 bytes");
}

#[test]
fn test_memory_savings_percentage() {
    let sizes = [100, 1_000, 10_000, 100_000, 1_000_000];
    
    println!("\nMemory Savings Summary:");
    println!("{:-<60}", "");
    println!("{:>10} {:>15} {:>15} {:>10} {:>10}", 
             "Size", "Bitfield", "Vec<bool>", "Ratio", "Savings");
    println!("{:-<60}", "");
    
    for size in sizes {
        let bf = Bitfield::new(size);
        let bf_memory = bf.memory_usage();
        let vec_memory = bf.vec_bool_memory_usage();
        let ratio = bf.memory_savings_ratio();
        let savings_pct = (1.0 - bf_memory as f64 / vec_memory as f64) * 100.0;
        
        println!(
            "{:>10} {:>15} {:>15} {:>10.2}x {:>9.1}%",
            size, bf_memory, vec_memory, ratio, savings_pct
        );
        
        // Verify at least 87% memory savings (7/8 reduction)
        assert!(savings_pct >= 87.0, "Should achieve at least 87% memory savings");
    }
    
    println!("{:-<60}", "");
}

#[test]
fn test_real_world_scenario() {
    // Simulate a real-world torrent with 50,000 pieces
    // (e.g., a 50GB torrent with 1MB piece size)
    let num_pieces = 50_000;
    
    let mut bf = Bitfield::new(num_pieces);
    
    // Simulate downloading 30% of pieces
    for i in (0..num_pieces).step_by(3) {
        bf.set(i).unwrap();
    }
    
    let bf_memory = bf.memory_usage();
    let vec_memory = bf.vec_bool_memory_usage();
    let ratio = bf.memory_savings_ratio();
    
    println!("\nReal-world scenario ({} pieces):", num_pieces);
    println!("  Downloaded: {} pieces", bf.count_set());
    println!("  Bitfield memory: {} bytes ({:.2} KB)", bf_memory, bf_memory as f64 / 1024.0);
    println!("  Vec<bool> memory: {} bytes ({:.2} KB)", vec_memory, vec_memory as f64 / 1024.0);
    println!("  Memory savings: {:.2}x", ratio);
    println!("  Savings percentage: {:.1}%", (1.0 - bf_memory as f64 / vec_memory as f64) * 100.0);
    
    // Verify correctness
    assert_eq!(bf.count_set(), 16_667, "Should have 16,667 pieces downloaded");
    assert!(ratio > 7.9, "Should achieve close to 8x memory savings");
}
