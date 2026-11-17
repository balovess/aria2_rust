use super::*;

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
