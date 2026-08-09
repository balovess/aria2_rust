// Tests for param_expander (extracted to keep main file under 600 lines).

// ======================================================================
// Test Group 1: Simple $num expansion
// ======================================================================

#[test]
fn test_simple_dollar_num_basic() {
    let uri = "http://example.com/file$3.txt";
    let expanded = expand_parameterized_uri(uri);

    assert!(!expanded.is_empty());
    assert!(expanded.len() > 1, "Should produce multiple URIs");
    assert!(
        expanded[0].contains("file1"),
        "First URI should contain file1"
    );
}

#[test]
fn test_simple_dollar_num_with_3() {
    let uri = "http://example.com/file$3.txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1000);
    assert!(expanded[0].ends_with("file1.txt"));
    assert!(expanded[1].ends_with("file2.txt"));
    assert!(expanded.last().unwrap().ends_with("file1000.txt"));
}

#[test]
fn test_simple_dollar_num_single_digit() {
    let uri = "http://example.com/file$1.txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 10);
    assert!(expanded[0].contains("file1"));
    assert!(expanded[9].contains("file10"));
}

// ======================================================================
// Test Group 2: Zero-padded ${num} expansion
// ======================================================================

#[test]
fn test_braced_zero_padded_single_number() {
    let uri = "http://example.com/file${03}.txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 3);
    assert_eq!(expanded[0], "http://example.com/file01.txt");
    assert_eq!(expanded[1], "http://example.com/file02.txt");
    assert_eq!(expanded[2], "http://example.com/file03.txt");
}

#[test]
fn test_braced_zero_padded_width_detection() {
    let uri = "http://example.com/data${0005}.bin";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 5);
    assert!(expanded[0].ends_with("data0001.bin"));
    assert!(expanded[4].ends_with("data0005.bin"));
    for uri in &expanded {
        assert!(
            uri.contains("data000") || uri.contains("data005"),
            "Should be zero-padded to width 4"
        );
    }
}

// ======================================================================
// Test Group 3: ${start-end} range forward
// ======================================================================

#[test]
fn test_braced_range_forward() {
    let uri = "http://example.com/chapter${01-05}.html";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 5);
    assert_eq!(expanded[0], "http://example.com/chapter01.html");
    assert_eq!(expanded[1], "http://example.com/chapter02.html");
    assert_eq!(expanded[2], "http://example.com/chapter03.html");
    assert_eq!(expanded[3], "http://example.com/chapter04.html");
    assert_eq!(expanded[4], "http://example.com/chapter05.html");
}

#[test]
fn test_braced_range_large_numbers() {
    let uri = "http://example.com/archive${100-105}.zip";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 6);
    assert_eq!(expanded[0], "http://example.com/archive100.zip");
    assert_eq!(expanded[5], "http://example.com/archive105.zip");
}

// ======================================================================
// Test Group 4: ${start-end:step} range with step
// ======================================================================

#[test]
fn test_braced_range_with_step() {
    let uri = "http://example.com/part${01-10:2}.dat";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 5);
    assert_eq!(expanded[0], "http://example.com/part01.dat");
    assert_eq!(expanded[1], "http://example.com/part03.dat");
    assert_eq!(expanded[2], "http://example.com/part05.dat");
    assert_eq!(expanded[3], "http://example.com/part07.dat");
    assert_eq!(expanded[4], "http://example.com/part09.dat");
}

#[test]
fn test_braced_step_of_3() {
    let uri = "http://example.com/img${001-009:3}.jpg";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 3);
    assert_eq!(expanded[0], "http://example.com/img001.jpg");
    assert_eq!(expanded[1], "http://example.com/img004.jpg");
    assert_eq!(expanded[2], "http://example.com/img007.jpg");
}

// ======================================================================
// Test Group 5: [FROM-TO] bracket syntax
// ======================================================================

#[test]
fn test_bracket_range_basic() {
    let uri = "http://example.com/file[01-05].zip";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 5);
    assert_eq!(expanded[0], "http://example.com/file01.zip");
    assert_eq!(expanded[1], "http://example.com/file02.zip");
    assert_eq!(expanded[2], "http://example.com/file03.zip");
    assert_eq!(expanded[3], "http://example.com/file04.zip");
    assert_eq!(expanded[4], "http://example.com/file05.zip");
}

#[test]
fn test_bracket_range_different_widths() {
    let uri = "http://example.com/data[1-10].bin";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 10);
    assert_eq!(expanded[0], "http://example.com/data01.bin");
    assert_eq!(expanded[9], "http://example.com/data10.bin");
}

// ======================================================================
// Test Group 6: [FROM-TO:STEP] bracket with step
// ======================================================================

#[test]
fn test_bracket_range_with_step() {
    let uri = "http://example.com/file[01-10:2].zip";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 5);
    assert_eq!(expanded[0], "http://example.com/file01.zip");
    assert_eq!(expanded[1], "http://example.com/file03.zip");
    assert_eq!(expanded[2], "http://example.com/file05.zip");
    assert_eq!(expanded[3], "http://example.com/file07.zip");
    assert_eq!(expanded[4], "http://example.com/file09.zip");
}

#[test]
fn test_bracket_step_of_5() {
    let uri = "http://example.com/vol[005-100:5].pdf";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 20);
    assert_eq!(expanded[0], "http://example.com/vol005.pdf");
    assert_eq!(expanded[1], "http://example.com/vol010.pdf");
    assert_eq!(expanded.last().unwrap(), &"http://example.com/vol100.pdf");
}

// ======================================================================
// Test Group 7: Reverse ranges [10-01]
// ======================================================================

#[test]
fn test_reverse_bracket_range() {
    let uri = "http://example.com/file[10-01].zip";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 10);
    assert_eq!(expanded[0], "http://example.com/file10.zip");
    assert_eq!(expanded[1], "http://example.com/file09.zip");
    assert_eq!(expanded[8], "http://example.com/file02.zip");
    assert_eq!(expanded[9], "http://example.com/file01.zip");
}

#[test]
fn test_reverse_braced_range() {
    let uri = "http://example.com/ch${10-05}.html";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 6);
    assert_eq!(expanded[0], "http://example.com/ch10.html");
    assert_eq!(expanded[5], "http://example.com/ch05.html");
}

// ======================================================================
// Test Group 8: Multiple patterns Cartesian product
// ======================================================================

#[test]
fn test_multiple_patterns_cartesian_product() {
    let uri_with_ranges = "http://example.com/${01-03}-${01-03}.html";
    let expanded = expand_parameterized_uri(uri_with_ranges);

    assert_eq!(expanded.len(), 9);
    assert_eq!(expanded[0], "http://example.com/01-01.html");
    assert_eq!(expanded[1], "http://example.com/01-02.html");
    assert_eq!(expanded[2], "http://example.com/01-03.html");
    assert_eq!(expanded[3], "http://example.com/02-01.html");
    assert_eq!(expanded[8], "http://example.com/03-03.html");
}

#[test]
fn test_three_patterns_cartesian() {
    let uri = "http://example.com/[1-2]-[a-d]-${01-02}.txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 16);
    assert_eq!(expanded[0], "http://example.com/1-a-01.txt");
    assert_eq!(expanded[15], "http://example.com/2-d-02.txt");
}

#[test]
fn test_mixed_brace_and_bracket() {
    let uri = "http://example.com/${01-02}-[01-03].dat";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 6);
    assert_eq!(expanded[0], "http://example.com/01-01.dat");
    assert_eq!(expanded[1], "http://example.com/01-02.dat");
    assert_eq!(expanded[2], "http://example.com/01-03.dat");
    assert_eq!(expanded[3], "http://example.com/02-01.dat");
    assert_eq!(expanded[5], "http://example.com/02-03.dat");
}

// ======================================================================
// Test Group 9: Choice and alphabetic ranges
// ======================================================================

#[test]
fn test_choice_expansion() {
    let expanded = expand_parameterized_uri("http://example.com/{a,b,c}.txt");
    assert_eq!(expanded, vec![
        "http://example.com/a.txt",
        "http://example.com/b.txt",
        "http://example.com/c.txt",
    ]);
}

#[test]
fn test_choice_cartesian_expansion() {
    let expanded = expand_parameterized_uri("http://example.com/{a,b}/{1,2}.txt");
    assert_eq!(expanded.len(), 4);
    assert_eq!(expanded[0], "http://example.com/a/1.txt");
    assert_eq!(expanded[3], "http://example.com/b/2.txt");
}

#[test]
fn test_alphabetic_range() {
    let expanded = expand_parameterized_uri("http://example.com/file[a-d].txt");
    assert_eq!(expanded, vec![
        "http://example.com/filea.txt",
        "http://example.com/fileb.txt",
        "http://example.com/filec.txt",
        "http://example.com/filed.txt",
    ]);
}

// ======================================================================
// Test Group 10: No-pattern passthrough
// ======================================================================

#[test]
fn test_no_pattern_passthrough() {
    let uris = vec![
        "http://example.com/normal_file.txt",
        "https://cdn.example.com/static/image.png",
        "ftp://files.example.com/document.pdf",
    ];

    for uri in uris {
        let expanded = expand_parameterized_uri(uri);
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0], uri);
    }
}

#[test]
fn test_uri_with_query_params_no_pattern() {
    let uri = "http://example.com/path?query=value&other=123";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], uri);
}

// ======================================================================
// Test Group 10: Edge cases
// ======================================================================

#[test]
fn test_single_value_range() {
    let uri = "http://example.com/file[5-5].txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], "http://example.com/file5.txt");
}

#[test]
fn test_single_value_braced() {
    let uri = "http://example.com/file${07-07}.txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], "http://example.com/file07.txt");
}

#[test]
fn test_large_numbers() {
    let uri = "http://example.com/big[099999-100005].bin";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 7);
    assert_eq!(expanded[0], "http://example.com/big099999.bin");
    assert_eq!(expanded[6], "http://example.com/big100005.bin");
}

#[test]
fn test_width_overflow_handling() {
    let uri = "http://example.com/f[1-100].txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 100);
    assert_eq!(expanded[0], "http://example.com/f001.txt");
    assert_eq!(expanded[99], "http://example.com/f100.txt");
}

#[test]
fn test_empty_uri() {
    let expanded = expand_parameterized_uri("");
    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], "");
}

// ======================================================================
// Test Group 11: Invalid patterns gracefully handled
// ======================================================================

#[test]
fn test_invalid_bracket_content() {
    let uri = "http://example.com/[abc-DEF].txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], uri);
}

#[test]
fn test_invalid_braced_content() {
    let uri = "http://example.com/${not-a-number}.txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], uri);
}

#[test]
fn test_zero_step_invalid() {
    let uri = "http://example.com/file[01-10:0].zip";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], uri);
}

#[test]
fn test_unclosed_braces() {
    let uri = "http://example.com/${unclosed.txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], uri);
}

#[test]
fn test_malformed_range() {
    let uri = "http://example.com/${10-}.txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], uri);
}

// ======================================================================
// Test Group 12: Special characters preserved
// ======================================================================

#[test]
fn test_special_chars_in_uri_preserved() {
    let uri = "http://example.com/path%20with%20spaces/${01-02}.html?query=test&special=%2F";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 2);
    assert_eq!(
        expanded[0],
        "http://example.com/path%20with%20spaces/01.html?query=test&special=%2F"
    );
    assert_eq!(
        expanded[1],
        "http://example.com/path%20with%20spaces/02.html?query=test&special=%2F"
    );
}

#[test]
fn test_uri_with_auth_and_port() {
    let uri = "http://user:pass@example.com:8080/files/${01-03}.dat";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 3);
    assert_eq!(
        expanded[0],
        "http://user:pass@example.com:8080/files/01.dat"
    );
    assert_eq!(
        expanded[2],
        "http://user:pass@example.com:8080/files/03.dat"
    );
}

#[test]
fn test_ipv6_address_not_confused() {
    let uri = "http://[2001:db8::1]:8080/file[01-02].txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 2);
    assert!(expanded[0].ends_with("file01.txt"));
    assert!(expanded[1].ends_with("file02.txt"));
}

#[test]
fn test_fragment_preserved() {
    let uri = "http://example.com/doc${01-02}.pdf#section=1";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 2);
    assert_eq!(expanded[0], "http://example.com/doc01.pdf#section=1");
    assert_eq!(expanded[1], "http://example.com/doc02.pdf#section=1");
}

// ======================================================================
// Additional edge case tests
// ======================================================================

#[test]
fn test_format_with_width_basic() {
    assert_eq!(format_with_width(1, 3), "001");
    assert_eq!(format_with_width(42, 5), "00042");
    assert_eq!(format_with_width(999, 2), "999");
    assert_eq!(format_with_width(0, 4), "0000");
}

#[test]
fn test_generate_range_forward() {
    let result = generate_range(1, 5, 1, 2);
    assert_eq!(result, vec!["01", "02", "03", "04", "05"]);
}

#[test]
fn test_generate_range_reverse() {
    let result = generate_range(5, 1, 1, 2);
    assert_eq!(result, vec!["05", "04", "03", "02", "01"]);
}

#[test]
fn test_generate_range_with_step() {
    let result = generate_range(1, 10, 3, 1);
    assert_eq!(result, vec!["1", "4", "7", "10"]);
}

#[test]
fn test_generate_range_single_value() {
    let result = generate_range(5, 5, 1, 3);
    assert_eq!(result, vec!["005"]);
}

#[test]
fn test_find_param_patterns_simple() {
    let patterns = find_param_patterns("http://ex.com/$2/file.txt");
    assert_eq!(patterns.len(), 1);
    match &patterns[0].1 {
        ParamPattern::Simple { value } => assert_eq!(*value, 2),
        _ => panic!("Expected Simple pattern"),
    }
}

#[test]
fn test_find_param_patterns_braced() {
    let patterns = find_param_patterns("http://ex.com/${01-05}.txt");
    assert_eq!(patterns.len(), 1);
    match &patterns[0].1 {
        ParamPattern::Braced { start, end, .. } => {
            assert_eq!(*start, 1);
            assert_eq!(*end, 5);
        }
        _ => panic!("Expected Braced pattern"),
    }
}

#[test]
fn test_find_param_patterns_bracket() {
    let patterns = find_param_patterns("http://ex.com/file[01-10].zip");
    assert_eq!(patterns.len(), 1);
    match &patterns[0].1 {
        ParamPattern::Bracket { start, end, .. } => {
            assert_eq!(*start, 1);
            assert_eq!(*end, 10);
        }
        _ => panic!("Expected Bracket pattern"),
    }
}

#[test]
fn test_complex_real_world_example() {
    let uri = "https://cdn.example.com/gallery/2024/photo${001-050}_hd.jpg";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 50);
    assert_eq!(
        expanded[0],
        "https://cdn.example.com/gallery/2024/photo001_hd.jpg"
    );
    assert_eq!(
        expanded[49],
        "https://cdn.example.com/gallery/2024/photo050_hd.jpg"
    );
}

#[test]
fn test_step_larger_than_range() {
    let uri = "http://example.com/f[01-05:10].txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 1);
    assert_eq!(expanded[0], "http://example.com/f01.txt");
}

#[test]
fn test_reverse_range_with_step() {
    let uri = "http://example.com/f[10-01:2].txt";
    let expanded = expand_parameterized_uri(uri);

    assert_eq!(expanded.len(), 5);
    assert_eq!(expanded[0], "http://example.com/f10.txt");
    assert_eq!(expanded[1], "http://example.com/f08.txt");
    assert_eq!(expanded[2], "http://example.com/f06.txt");
    assert_eq!(expanded[3], "http://example.com/f04.txt");
    assert_eq!(expanded[4], "http://example.com/f02.txt");
}
