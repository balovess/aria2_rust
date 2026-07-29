//! Tests for Content-Disposition header parsing.

use super::encoding::{is_dir_traversal, iso8859p1_to_utf8};
use super::parser::hex_digit;
use super::{parse_content_disposition, extract_filename};

// -- Basic disposition type parsing --

#[test]
fn test_attachment_disposition() {
    let result = parse_content_disposition("attachment");
    assert_eq!(result.disposition_type, "attachment");
    assert!(result.filename.is_none());
    assert!(result.filename_ascii.is_none());
}

#[test]
fn test_inline_disposition() {
    let result = parse_content_disposition("inline");
    assert_eq!(result.disposition_type, "inline");
    assert!(result.filename.is_none());
}

#[test]
fn test_form_data_disposition() {
    let result = parse_content_disposition("form-data");
    assert_eq!(result.disposition_type, "form-data");
}

#[test]
fn test_disposition_type_with_trailing_whitespace() {
    let result = parse_content_disposition("attachment  ");
    assert_eq!(result.disposition_type, "attachment");
}

#[test]
fn test_disposition_type_with_leading_whitespace() {
    let result = parse_content_disposition("  attachment");
    assert_eq!(result.disposition_type, "attachment");
}

// -- filename= (unquoted) --

#[test]
fn test_unquoted_filename() {
    let result = parse_content_disposition("attachment; filename=example.html");
    assert_eq!(result.disposition_type, "attachment");
    assert_eq!(result.filename.as_deref(), Some("example.html"));
    assert_eq!(result.filename_ascii.as_deref(), Some("example.html"));
}

#[test]
fn test_unquoted_filename_with_spaces_before_equals() {
    let result = parse_content_disposition("attachment; filename =example.html");
    assert_eq!(result.filename.as_deref(), Some("example.html"));
}

// -- filename= (quoted) --

#[test]
fn test_quoted_filename() {
    let result = parse_content_disposition("attachment; filename=\"example.html\"");
    assert_eq!(result.filename.as_deref(), Some("example.html"));
}

#[test]
fn test_quoted_filename_with_spaces() {
    let result = parse_content_disposition("attachment; filename = \"example.html\"");
    assert_eq!(result.filename.as_deref(), Some("example.html"));
}

#[test]
fn test_quoted_filename_with_escaped_quote() {
    // The C++ parser uses backslash-escaping in quoted strings
    let result = parse_content_disposition("attachment; filename=\"example\\\"file.html\"");
    assert_eq!(result.filename.as_deref(), Some("example\"file.html"));
}

// -- filename*= (RFC 5987 / RFC 6266) --

#[test]
fn test_ext_filename_utf8() {
    let result = parse_content_disposition(
        "attachment; filename*=UTF-8''%e3%81%93%e3%82%93%e3%81%ab%e3%81%a1%e3%81%af.txt",
    );
    assert_eq!(result.disposition_type, "attachment");
    assert_eq!(
        result.filename.as_deref(),
        Some("\u{3053}\u{3093}\u{306b}\u{3061}\u{306f}.txt")
    );
}

#[test]
fn test_ext_filename_utf8_simple() {
    let result = parse_content_disposition("attachment; filename*=UTF-8''hello.txt");
    assert_eq!(result.filename.as_deref(), Some("hello.txt"));
}

#[test]
fn test_ext_filename_iso_8859_1() {
    // e9 is é in ISO-8859-1
    let result = parse_content_disposition("attachment; filename*=ISO-8859-1''%e9");
    assert_eq!(result.filename.as_deref(), Some("\u{e9}"));
}

#[test]
fn test_ext_filename_takes_priority_over_filename() {
    let result = parse_content_disposition(
        "attachment; filename=\"fallback.txt\"; filename*=UTF-8''preferred.txt",
    );
    assert_eq!(result.filename.as_deref(), Some("preferred.txt"));
    assert_eq!(result.filename_ascii.as_deref(), Some("fallback.txt"));
}

#[test]
fn test_filename_star_before_filename_still_wins() {
    let result =
        parse_content_disposition("attachment; filename*=UTF-8''star.txt; filename=plain.txt");
    // filename* takes priority per RFC 6266
    assert_eq!(result.filename.as_deref(), Some("star.txt"));
    assert_eq!(result.filename_ascii.as_deref(), Some("plain.txt"));
}

#[test]
fn test_ext_filename_with_language() {
    let result = parse_content_disposition("attachment; filename*=UTF-8'en'test.txt");
    assert_eq!(result.filename.as_deref(), Some("test.txt"));
}

// -- Duplicate parameter handling --

#[test]
fn test_duplicate_filename_is_rejected() {
    let result =
        parse_content_disposition("attachment; filename=first.txt; filename=second.txt");
    // Duplicate filename= should cause parse failure
    assert_eq!(result.disposition_type, "");
    assert!(result.filename.is_none());
}

#[test]
fn test_duplicate_ext_filename_is_rejected() {
    let result = parse_content_disposition(
        "attachment; filename*=UTF-8''first.txt; filename*=UTF-8''second.txt",
    );
    assert_eq!(result.disposition_type, "");
    assert!(result.filename.is_none());
}

// -- Directory traversal rejection --

#[test]
fn test_dot_filename_rejected() {
    let result = parse_content_disposition("attachment; filename=.");
    assert!(result.filename.is_none());
}

#[test]
fn test_dotdot_filename_rejected() {
    let result = parse_content_disposition("attachment; filename=..");
    assert!(result.filename.is_none());
}

#[test]
fn test_absolute_path_filename_rejected() {
    let result = parse_content_disposition("attachment; filename=/etc/passwd");
    assert!(result.filename.is_none());
}

#[test]
fn test_dot_slash_filename_rejected() {
    let result = parse_content_disposition("attachment; filename=./secret");
    assert!(result.filename.is_none());
}

#[test]
fn test_dotdot_slash_filename_rejected() {
    let result = parse_content_disposition("attachment; filename=../secret");
    assert!(result.filename.is_none());
}

#[test]
fn test_path_with_dot_component_rejected() {
    let result = parse_content_disposition("attachment; filename=dir/./file.txt");
    assert!(result.filename.is_none());
}

#[test]
fn test_path_with_dotdot_component_rejected() {
    let result = parse_content_disposition("attachment; filename=dir/../file.txt");
    assert!(result.filename.is_none());
}

#[test]
fn test_trailing_slash_rejected() {
    let result = parse_content_disposition("attachment; filename=dir/");
    assert!(result.filename.is_none());
}

#[test]
fn test_trailing_dot_rejected() {
    let result = parse_content_disposition("attachment; filename=dir/.");
    assert!(result.filename.is_none());
}

#[test]
fn test_trailing_dotdot_rejected() {
    let result = parse_content_disposition("attachment; filename=dir/..");
    assert!(result.filename.is_none());
}

#[test]
fn test_backslash_filename_rejected() {
    let result = parse_content_disposition("attachment; filename=dir\\file.txt");
    assert!(result.filename.is_none());
}

#[test]
fn test_control_char_filename_rejected() {
    let result = parse_content_disposition("attachment; filename=\"hello\x01world\"");
    assert!(result.filename.is_none());
}

// -- Invalid input --

#[test]
fn test_empty_input() {
    let result = parse_content_disposition("");
    assert_eq!(result.disposition_type, "");
    assert!(result.filename.is_none());
}

#[test]
fn test_only_whitespace() {
    let result = parse_content_disposition("   ");
    assert_eq!(result.disposition_type, "");
    assert!(result.filename.is_none());
}

#[test]
fn test_invalid_char_in_disposition_type() {
    let result = parse_content_disposition("attach@ment");
    assert_eq!(result.disposition_type, "");
}

// -- extract_filename convenience --

#[test]
fn test_extract_filename_basic() {
    assert_eq!(
        extract_filename("attachment; filename=report.pdf"),
        Some("report.pdf".to_owned())
    );
}

#[test]
fn test_extract_filename_none() {
    assert_eq!(extract_filename("inline"), None);
}

#[test]
fn test_extract_filename_with_ext() {
    assert_eq!(
        extract_filename("attachment; filename*=UTF-8''%c3%a9.txt"),
        Some("\u{e9}.txt".to_owned())
    );
}

// -- Multiple parameters --

#[test]
fn test_multiple_parameters() {
    let result = parse_content_disposition(
        "attachment; size=1234; filename=\"test.txt\"; creation-date=\"Wed, 12 Feb 1997 16:29:51 -0500\"",
    );
    assert_eq!(result.disposition_type, "attachment");
    assert_eq!(result.filename.as_deref(), Some("test.txt"));
}

// -- Case insensitivity --

#[test]
fn test_case_insensitive_filename_param() {
    let result = parse_content_disposition("attachment; FILENAME=test.txt");
    assert_eq!(result.filename.as_deref(), Some("test.txt"));
}

#[test]
fn test_case_insensitive_filename_star_param() {
    let result = parse_content_disposition("attachment; FILENAME*=utf-8''test.txt");
    assert_eq!(result.filename.as_deref(), Some("test.txt"));
}

// -- ISO-8859-1 conversion --

#[test]
fn test_iso8859p1_to_utf8_ascii() {
    assert_eq!(iso8859p1_to_utf8(b"hello"), Some("hello".to_owned()));
}

#[test]
fn test_iso8859p1_to_utf8_extended() {
    // 0xE9 = é in ISO-8859-1 → UTF-8: 0xC3 0xA9
    let result = iso8859p1_to_utf8(&[0xE9]).unwrap();
    assert_eq!(result, "\u{e9}");
}

#[test]
fn test_iso8859p1_to_utf8_c1_control_rejected() {
    // 0x80..0x9F are C1 control characters, rejected per C++ implementation
    assert!(iso8859p1_to_utf8(&[0x80]).is_none());
    assert!(iso8859p1_to_utf8(&[0x9F]).is_none());
}

#[test]
fn test_iso8859p1_to_utf8_nbsp() {
    // 0xA0 = non-breaking space → UTF-8: 0xC2 0xA0
    let result = iso8859p1_to_utf8(&[0xA0]).unwrap();
    assert_eq!(result, "\u{a0}");
}

// -- hex_digit helper --

#[test]
fn test_hex_digit() {
    assert_eq!(hex_digit(b'0'), Some(0));
    assert_eq!(hex_digit(b'9'), Some(9));
    assert_eq!(hex_digit(b'a'), Some(10));
    assert_eq!(hex_digit(b'f'), Some(15));
    assert_eq!(hex_digit(b'A'), Some(10));
    assert_eq!(hex_digit(b'F'), Some(15));
    assert_eq!(hex_digit(b'g'), None);
    assert_eq!(hex_digit(b' '), None);
}

// -- is_dir_traversal --

#[test]
fn test_dir_traversal_patterns() {
    assert!(is_dir_traversal("."));
    assert!(is_dir_traversal(".."));
    assert!(is_dir_traversal("/"));
    assert!(is_dir_traversal("/etc/passwd"));
    assert!(is_dir_traversal("./secret"));
    assert!(is_dir_traversal("../secret"));
    assert!(is_dir_traversal("dir/./file"));
    assert!(is_dir_traversal("dir/../file"));
    assert!(is_dir_traversal("dir/"));
    assert!(is_dir_traversal("dir/."));
    assert!(is_dir_traversal("dir/.."));
    assert!(is_dir_traversal("dir\\file"));
    assert!(is_dir_traversal("\x01bad"));
}

#[test]
fn test_valid_filenames_not_traversal() {
    assert!(!is_dir_traversal("file.txt"));
    assert!(!is_dir_traversal("hello world.pdf"));
    assert!(!is_dir_traversal("archive.tar.gz"));
    assert!(!is_dir_traversal(""));
}

// -- Real-world header values --

#[test]
fn test_real_world_attachment() {
    let result = parse_content_disposition(
        "attachment; filename=\"genome.jpeg\"; modification-date=\"Wed, 12 Feb 1997 16:29:51 -0500\";",
    );
    assert_eq!(result.disposition_type, "attachment");
    assert_eq!(result.filename.as_deref(), Some("genome.jpeg"));
}

#[test]
fn test_real_world_utf8_filename_star() {
    let result = parse_content_disposition(
        "attachment; filename=\"hello.pdf\"; filename*=UTF-8''%e2%82%ac%20rates.pdf",
    );
    assert_eq!(result.filename.as_deref(), Some("\u{20ac} rates.pdf"));
    assert_eq!(result.filename_ascii.as_deref(), Some("hello.pdf"));
}

// -- Only filename* present (no filename=) --

#[test]
fn test_only_ext_filename() {
    let result = parse_content_disposition("attachment; filename*=UTF-8''test.txt");
    assert_eq!(result.filename.as_deref(), Some("test.txt"));
    assert!(result.filename_ascii.is_none());
}

// -- Only filename= present (no filename*) --

#[test]
fn test_only_plain_filename() {
    let result = parse_content_disposition("attachment; filename=test.txt");
    assert_eq!(result.filename.as_deref(), Some("test.txt"));
    assert_eq!(result.filename_ascii.as_deref(), Some("test.txt"));
}

// -- Non-filename parameters are ignored --

#[test]
fn test_non_filename_params_ignored() {
    let result =
        parse_content_disposition("form-data; name=\"fieldName\"; filename=\"file.dat\"");
    assert_eq!(result.disposition_type, "form-data");
    assert_eq!(result.filename.as_deref(), Some("file.dat"));
}

// -- Percent-encoding in filename* with multi-byte UTF-8 --

#[test]
fn test_percent_encoded_cjk() {
    // 日本語 in UTF-8: e6 97 a5 e6 9c ac e8 aa 9e
    let result = parse_content_disposition(
        "attachment; filename*=UTF-8''%e6%97%a5%e6%9c%ac%e8%aa%9e.txt",
    );
    assert_eq!(
        result.filename.as_deref(),
        Some("\u{65e5}\u{672c}\u{8a9e}.txt")
    );
}

// -- Filename with special token characters --

#[test]
fn test_filename_with_token_chars() {
    let result = parse_content_disposition("attachment; filename=file-v1.2_beta.txt");
    assert_eq!(result.filename.as_deref(), Some("file-v1.2_beta.txt"));
}

// -- Quoted string with backslash-escaped characters --

#[test]
fn test_quoted_backslash_escape() {
    // The parser correctly unescapes \\ to \ in quoted strings,
    // but filenames containing backslashes are rejected by the
    // directory-traversal check (is_dir_traversal rejects '\').
    let result = parse_content_disposition("attachment; filename=\"path\\\\to\\\\file.txt\"");
    assert_eq!(
        result.filename, None,
        "Backslash in filename should be rejected by dir traversal"
    );
}

#[test]
fn test_quoted_escaped_backslash_then_char() {
    // \n in a quoted string is just the literal characters 'n' after backslash
    let result = parse_content_disposition("attachment; filename=\"hello\\nworld.txt\"");
    assert_eq!(result.filename.as_deref(), Some("hellonworld.txt"));
}

// -- Edge: filename= is ignored when filename* already found --

#[test]
fn test_filename_ignored_when_ext_already_found() {
    let result =
        parse_content_disposition("attachment; filename*=UTF-8''star.txt; filename=plain.txt");
    // RFC 6266: filename* takes priority for the `filename` field,
    // but filename= is still collected as filename_ascii fallback.
    assert_eq!(result.filename.as_deref(), Some("star.txt"));
    assert_eq!(result.filename_ascii.as_deref(), Some("plain.txt"));
}

// -- Quoted string with space --

#[test]
fn test_quoted_filename_with_spaces_in_value() {
    let result = parse_content_disposition("attachment; filename=\"my report.pdf\"");
    assert_eq!(result.filename.as_deref(), Some("my report.pdf"));
}

// -- Whitespace handling around semicolons --

#[test]
fn test_whitespace_around_semicolons() {
    let result = parse_content_disposition("attachment ; filename=test.txt");
    assert_eq!(result.filename.as_deref(), Some("test.txt"));
}
