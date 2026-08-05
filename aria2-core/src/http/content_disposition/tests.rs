//! Tests for Content-Disposition header parsing.

use super::encoding::{is_dir_traversal, iso8859p1_to_utf8};
use super::parser::hex_digit;
use super::{extract_filename, parse_content_disposition};

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
    let result = parse_content_disposition("attachment; filename=first.txt; filename=second.txt");
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
        "attachment; filename=\"genome.jpeg\"; modification-date=\"Wed, 12 Feb 1997 16:29:51 -0500\"",
    );
    assert_eq!(result.disposition_type, "attachment");
    assert_eq!(result.filename.as_deref(), Some("genome.jpeg"));
}

#[test]
fn test_real_world_attachment_with_trailing_semicolon_rejected() {
    let result = parse_content_disposition(
        "attachment; filename=\"genome.jpeg\"; modification-date=\"Wed, 12 Feb 1997 16:29:51 -0500\";",
    );
    assert_eq!(result.disposition_type, "");
    assert!(result.filename.is_none());
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
    let result = parse_content_disposition("form-data; name=\"fieldName\"; filename=\"file.dat\"");
    assert_eq!(result.disposition_type, "form-data");
    assert_eq!(result.filename.as_deref(), Some("file.dat"));
}

// -- Percent-encoding in filename* with multi-byte UTF-8 --

#[test]
fn test_percent_encoded_cjk() {
    // 日本語 in UTF-8: e6 97 a5 e6 9c ac e8 aa 9e
    let result =
        parse_content_disposition("attachment; filename*=UTF-8''%e6%97%a5%e6%9c%ac%e8%aa%9e.txt");
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

// ===========================================================================
// C++ parity: edge cases verified against aria2 `src/util.cc`
// (`parse_content_disposition`) and its CppUnit suite
// (`test/UtilTest1.cc::testParseContentDisposition{1,2}`).
//
// Each test names the corresponding upstream case where one exists.
// ===========================================================================

/// Assert the header is rejected outright (C++ returns -1).
fn assert_rejected(header: &str) {
    let result = parse_content_disposition(header);
    assert_eq!(
        result.disposition_type, "",
        "expected `{header}` to be rejected (C++ returns -1)"
    );
    assert!(
        result.filename.is_none(),
        "expected `{header}` to yield no filename"
    );
    assert!(
        result.filename_ascii.is_none(),
        "expected `{header}` to yield no ascii filename"
    );
}

/// Assert the header parses but carries no usable filename (C++ returns 0).
fn assert_accepted_without_filename(header: &str, disposition_type: &str) {
    let result = parse_content_disposition(header);
    assert_eq!(
        result.disposition_type, disposition_type,
        "expected `{header}` to parse successfully"
    );
    assert!(
        result.filename.is_none(),
        "expected `{header}` to yield no filename, got {:?}",
        result.filename
    );
}

// -- Trailing `;` / empty final parameter: rejected for C++ parity --------

#[test]
fn test_trailing_semicolon_after_token_rejected() {
    assert_rejected("attachment; filename=foo.html ;");
}

#[test]
fn test_trailing_semicolon_without_space_rejected() {
    assert_rejected("attachment; filename=foo.html;");
}

#[test]
fn test_trailing_semicolon_after_quoted_string_rejected() {
    assert_rejected("attachment; filename=\"foo.html\";");
}

#[test]
fn test_trailing_semicolon_after_ext_value_rejected() {
    assert_rejected("attachment; filename*=UTF-8''foo.html;");
}

#[test]
fn test_bare_trailing_semicolon_after_disposition_type_rejected() {
    assert_rejected("attachment;");
}

#[test]
fn test_trailing_semicolon_with_whitespace_rejected() {
    assert_rejected("attachment; filename=foo.html ;   ");
}

#[test]
fn test_empty_parameter_in_middle_rejected() {
    // C++ `attemptyparam`: "attachment; ;filename=foo" -> -1
    assert_rejected("attachment; ;filename=foo");
}

// -- Empty / missing values -------------------------------------------------

#[test]
fn test_empty_filename_value_at_end_rejected() {
    // C++ aria2 original case: "zero-length filename. token cannot be empty,
    // so this is invalid." -> -1
    assert_rejected("attachment; filename=");
}

#[test]
fn test_empty_filename_value_before_semicolon_rejected() {
    // C++ aria2 original case: "empty value is not allowed" -> -1
    assert_rejected("attachment; filename=;");
}

#[test]
fn test_empty_filename_value_followed_by_param_rejected() {
    assert_rejected("attachment; filename=; foo=bar");
}

#[test]
fn test_empty_quoted_filename_accepted_as_no_filename() {
    // C++ aria2 original case: "quoted-string can be empty string, so this is
    // ok" -> returns 0, i.e. header parses but the filename is empty.
    assert_accepted_without_filename("attachment; filename=\"\"", "attachment");
}

#[test]
fn test_empty_ext_filename_value_accepted_as_no_filename() {
    // C++ aria2 original case: "value-chars is *(pct-encoded / attr-char), so
    // empty string is allowed" -> returns 0.
    assert_accepted_without_filename("attachment; filename*=UTF-8''", "attachment");
}

#[test]
fn test_unterminated_quoted_filename_rejected() {
    // C++ `attbrokenquotedfn2`: "attachment; filename=\"bar" -> -1
    assert_rejected("attachment; filename=\"bar");
    assert_rejected("attachment; filename=\"");
}

// -- Parameter without `=` --------------------------------------------------

#[test]
fn test_filename_parameter_without_equals_rejected() {
    // `;` is not an RFC 2616 token char, so CD_DISPOSITION_PARM_NAME bails out.
    assert_rejected("attachment; filename; x=y");
}

#[test]
fn test_filename_parameter_without_equals_at_end_rejected() {
    // Ends in CD_DISPOSITION_PARM_NAME, which C++ rejects at EOF.
    assert_rejected("attachment; filename");
}

#[test]
fn test_parameter_without_equals_after_whitespace_rejected() {
    // Ends in CD_AFTER_DISPOSITION_PARM_NAME, also rejected at EOF.
    assert_rejected("attachment; filename ");
}

#[test]
fn test_disposition_type_without_delimiter_rejected() {
    // C++ `attmissingdelim3`: "attachment filename=bar" -> -1
    assert_rejected("attachment filename=bar");
}

// -- Multiple `filename` parameters (value-completion semantics) ------------

#[test]
fn test_two_filenames_rejected() {
    // C++ `attwith2filenames`:
    // "attachment; filename=\"foo.html\"; filename=\"bar.html\"" -> -1
    assert_rejected("attachment; filename=\"foo.html\"; filename=\"bar.html\"");
}

#[test]
fn test_repeated_filename_after_ext_filename_is_not_a_duplicate() {
    // In C++ a `filename=` that follows an accepted `filename*=` has
    // in_file_parm == 0, so CD_FILENAME_FOUND is never raised and further
    // `filename=` parameters do NOT trigger the duplicate check.
    let result = parse_content_disposition(
        "attachment; filename*=UTF-8''star.txt; filename=a.txt; filename=b.txt",
    );
    assert_eq!(result.disposition_type, "attachment");
    // filename* still wins for the primary filename, exactly like C++.
    assert_eq!(result.filename.as_deref(), Some("star.txt"));
}

#[test]
fn test_duplicate_filename_before_ext_filename_still_rejected() {
    // Both `filename=` values complete before any `filename*`, so the
    // duplicate check fires just like in C++.
    assert_rejected("attachment; filename=a.txt; filename=b.txt; filename*=UTF-8''c.txt");
}

#[test]
fn test_ext_filename_after_filename_wins() {
    // C++ `attfnboth`:
    // "attachment; filename=\"foo-ae.html\"; filename*=UTF-8''foo-%c3%a4.html"
    let result = parse_content_disposition(
        "attachment; filename=\"foo-ae.html\"; filename*=UTF-8''foo-%c3%a4.html",
    );
    assert_eq!(result.filename.as_deref(), Some("foo-\u{e4}.html"));
}

#[test]
fn test_ext_filename_before_filename_wins() {
    // C++ `attfnboth2`:
    // "attachment; filename*=UTF-8''foo-%c3%a4.html; filename=\"foo-ae.html\""
    let result = parse_content_disposition(
        "attachment; filename*=UTF-8''foo-%c3%a4.html; filename=\"foo-ae.html\"",
    );
    assert_eq!(result.filename.as_deref(), Some("foo-\u{e4}.html"));
}

// -- ext-token detection ----------------------------------------------------

#[test]
fn test_bare_star_parameter_is_not_an_ext_token() {
    // C++ requires `mark_first != mark_last - 1` before treating a trailing
    // '*' as an ext-token, so a parameter literally named "*" takes the
    // ordinary value path and the header stays valid.
    let result = parse_content_disposition("attachment; *=foo; filename=bar.txt");
    assert_eq!(result.disposition_type, "attachment");
    assert_eq!(result.filename.as_deref(), Some("bar.txt"));
}

#[test]
fn test_multi_star_ext_token_is_ignored() {
    // C++ `attfnboth3`: filename*0* is an ext-token but not `filename*`, so
    // only the real filename* contributes; its charset is the one reported.
    let result = parse_content_disposition(
        "attachment; filename*0*=ISO-8859-15''euro-sign%3d%a4; \
         filename*=ISO-8859-1''currency-sign%3d%a4",
    );
    assert_eq!(result.disposition_type, "attachment");
    assert_eq!(result.filename.as_deref(), Some("currency-sign=\u{a4}"));
}

#[test]
fn test_ext_filename_whitespace_before_equals_accepted() {
    // C++ `attwithfn2231ws3`: "attachment; filename* =UTF-8''foo-%c3%a4.html"
    let result = parse_content_disposition("attachment; filename* =UTF-8''foo-%c3%a4.html");
    assert_eq!(result.filename.as_deref(), Some("foo-\u{e4}.html"));
}

#[test]
fn test_ext_filename_whitespace_after_equals_accepted() {
    // C++ `attwithfn2231ws2`: "attachment; filename*= UTF-8''foo-%c3%a4.html"
    let result = parse_content_disposition("attachment; filename*= UTF-8''foo-%c3%a4.html");
    assert_eq!(result.filename.as_deref(), Some("foo-\u{e4}.html"));
}

#[test]
fn test_ext_filename_whitespace_inside_name_rejected() {
    // C++ `attwithfn2231ws1`: "attachment; filename *=UTF-8''foo-%c3%a4.html" -> -1
    assert_rejected("attachment; filename *=UTF-8''foo-%c3%a4.html");
}

// -- Charset validation of ext-values --------------------------------------

#[test]
fn test_iso_8859_1_ext_value_accepts_high_bytes() {
    // C++ `attwithisofn2231iso`: "attachment; filename*=iso-8859-1''foo-%E4.html"
    let result = parse_content_disposition("attachment; filename*=iso-8859-1''foo-%E4.html");
    assert_eq!(result.filename.as_deref(), Some("foo-\u{e4}.html"));
}

#[test]
fn test_iso_8859_1_ext_value_rejects_c1_control_octet() {
    // C++ `attwithfn2231utf8-bad`: the %82 octet is not in ISO-8859-1, so the
    // whole header is rejected (-1) rather than merely losing the filename.
    assert_rejected("attachment; filename*=iso-8859-1''foo-%c3%a4-%e2%82%ac.html");
}

#[test]
fn test_utf8_ext_value_rejects_invalid_sequence() {
    // C++ `attwithfn2231iso-bad`: "attachment; filename*=utf-8''foo-%E4.html" -> -1
    assert_rejected("attachment; filename*=utf-8''foo-%E4.html");
}

#[test]
fn test_utf8_ext_value_rejects_truncated_sequence_at_end() {
    // The C++ DFA is not in UTF8_ACCEPT at EOF, so CD_VALUE_CHARS returns -1.
    assert_rejected("attachment; filename*=utf-8''%c3");
}

#[test]
fn test_utf8_ext_value_rejects_truncated_sequence_before_semicolon() {
    assert_rejected("attachment; filename*=utf-8''%c3; foo=bar");
}

#[test]
fn test_invalid_utf8_in_non_filename_ext_param_rejects_header() {
    // C++ runs the UTF-8 DFA outside the `in_file_parm` guard, so a bad
    // ext-value on an unrelated parameter still fails the whole header.
    assert_rejected("attachment; foo*=utf-8''%e4; filename=ok.txt");
}

#[test]
fn test_utf8_ext_value_accepts_combining_sequence() {
    // C++ `attwithfn2231utf8comp`: "attachment; filename*=UTF-8''foo-a%cc%88.html"
    let result = parse_content_disposition("attachment; filename*=UTF-8''foo-a%cc%88.html");
    assert_eq!(result.filename.as_deref(), Some("foo-a\u{308}.html"));
}

#[test]
fn test_ext_value_missing_charset_rejected() {
    // C++ `attwithfn2231noc`: "attachment; filename*=''foo-..." -> -1
    assert_rejected("attachment; filename*=''foo-%c3%a4-%e2%82%ac.html");
}

#[test]
fn test_ext_value_missing_second_quote_rejected() {
    // C++ `attwithfn2231singleqmissing`
    assert_rejected("attachment; filename*=UTF-8'foo-%c3%a4.html");
}

#[test]
fn test_ext_value_quoted_rejected() {
    // C++ `attwithfn2231quot` / `attwithfn2231quot2`
    assert_rejected("attachment; filename*=\"UTF-8''foo-%c3%a4.html\"");
    assert_rejected("attachment; filename*=\"foo%20bar.html\"");
}

#[test]
fn test_ext_value_bad_percent_encoding_rejected() {
    // C++ `attwithfn2231nbadpct1` / `attwithfn2231nbadpct2`
    assert_rejected("attachment; filename*=UTF-8''foo%");
    assert_rejected("attachment; filename*=UTF-8''f%oo.html");
}

#[test]
fn test_ext_value_double_percent_encoding_preserved() {
    // C++ `attwithfn2231dpct`: "attachment; filename*=UTF-8''A-%2541.html"
    let result = parse_content_disposition("attachment; filename*=UTF-8''A-%2541.html");
    assert_eq!(result.filename.as_deref(), Some("A-%41.html"));
}

// -- Assorted greenbytes tc2231 cases --------------------------------------

#[test]
fn test_quoted_disposition_type_rejected() {
    // C++ `inlonlyquoted`
    assert_rejected("\"inline\"");
}

#[test]
fn test_missing_disposition_type_rejected() {
    // C++ `attmissingdisposition` / `attreversed` / `emptydisposition`
    assert_rejected("filename=foo.html");
    assert_rejected("x=y; filename=foo.html");
    assert_rejected("filename=foo.html; attachment");
    assert_rejected("; filename=foo.html");
}

#[test]
fn test_two_disposition_types_rejected() {
    // C++ `attandinline` / `attandinline2`
    assert_rejected("inline; attachment; filename=foo.html");
    assert_rejected("attachment; inline; filename=foo.html");
}

#[test]
fn test_broken_token_filenames_rejected() {
    // C++ `attwithtokfncommanq`, `attfnbrokentoken`, `attwithasciifilenamenqws`,
    // `attbrokenquotedfn`, `attbrokenquotedfn3`, `attmultinstances`
    assert_rejected("attachment; filename=foo,bar.html");
    assert_rejected("attachment; filename=foo[1](2).html");
    assert_rejected("attachment; filename=foo bar.html");
    assert_rejected("attachment; filename=\"foo.html\".txt");
    assert_rejected("attachment; filename=foo\"bar;baz\"qux");
    assert_rejected("attachment; filename=foo.html, attachment; filename=bar.html");
}

#[test]
fn test_missing_delimiter_between_params_rejected() {
    // C++ `attmissingdelim` / `attmissingdelim2`
    assert_rejected("attachment; foo=foo filename=bar");
    assert_rejected("attachment; filename=bar foo=foo ");
}

#[test]
fn test_rfc2047_token_rejected_but_quoted_accepted() {
    // C++ `attrfc2047token` -> -1, `attrfc2047quoted` -> literal value.
    assert_rejected("attachment; filename==?ISO-8859-1?Q?foo-=E4.html?=");
    let result =
        parse_content_disposition("attachment; filename=\"=?ISO-8859-1?Q?foo-=E4.html?=\"");
    assert_eq!(
        result.filename.as_deref(),
        Some("=?ISO-8859-1?Q?foo-=E4.html?=")
    );
}

#[test]
fn test_non_filename_params_parse_without_filename() {
    // C++ `attconfusedparam`, `attcdate`, `dispext`, `dispextbadfn`, `attwithnamepct`
    assert_accepted_without_filename("attachment; xfilename=foo.html", "attachment");
    assert_accepted_without_filename(
        "attachment; creation-date=\"Wed, 12 Feb 1997 16:29:51 -0500\"",
        "attachment",
    );
    assert_accepted_without_filename("foobar", "foobar");
    assert_accepted_without_filename("attachment; example=\"filename=example.txt\"", "attachment");
    assert_accepted_without_filename("attachment; name=\"foo-%41.html\"", "attachment");
}

#[test]
fn test_single_quoted_token_filename_kept_verbatim() {
    // C++ `attwithfntokensq`: "attachment; filename='foo.bar'" -> "'foo.bar'"
    let result = parse_content_disposition("attachment; filename='foo.bar'");
    assert_eq!(result.filename.as_deref(), Some("'foo.bar'"));
}

#[test]
fn test_percent_sequences_in_quoted_filename_not_decoded() {
    // C++ `attwithfnrawpctenca` / `attwithfnusingpct` / `attwithfnrawpctencaq`
    let result = parse_content_disposition("attachment; filename=\"foo-%41.html\"");
    assert_eq!(result.filename.as_deref(), Some("foo-%41.html"));

    let result = parse_content_disposition("attachment; filename=\"50%.html\"");
    assert_eq!(result.filename.as_deref(), Some("50%.html"));

    let result = parse_content_disposition("attachment; filename=\"foo-%\\41.html\"");
    assert_eq!(result.filename.as_deref(), Some("foo-%41.html"));
}

#[test]
fn test_escaped_quotes_in_quoted_filename() {
    // C++ `attwithasciifnescapedquote` / `attwithquotedsemicolon`
    let result = parse_content_disposition("attachment; filename=\"\\\"quoting\\\" tested.html\"");
    assert_eq!(result.filename.as_deref(), Some("\"quoting\" tested.html"));

    let result = parse_content_disposition("attachment; filename=\"Here's a semicolon;.html\"");
    assert_eq!(result.filename.as_deref(), Some("Here's a semicolon;.html"));
}

#[test]
fn test_quoted_filename_preserves_surrounding_spaces() {
    // C++ getContentDispositionFilename: "attachment; filename= \" aria2.tar.bz2 \""
    let result = parse_content_disposition("attachment; filename= \" aria2.tar.bz2 \"");
    assert_eq!(result.filename.as_deref(), Some(" aria2.tar.bz2 "));
}

#[test]
fn test_ext_value_disguised_absolute_path_rejected_by_traversal_check() {
    // C++ `attwithfn2231abspathdisguised` parses fine (returns "\foo.html"),
    // but getContentDispositionFilename() then drops it because of the
    // backslash. The header itself must still parse.
    let result = parse_content_disposition("attachment; filename*=UTF-8''%5cfoo.html");
    assert_eq!(result.disposition_type, "attachment");
    assert!(result.filename.is_none());
}
