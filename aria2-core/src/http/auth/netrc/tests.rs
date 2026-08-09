//! Unit tests for the netrc module.

use std::path::Path;

use super::error::NetrcError;
use super::parser::NetrcParser;
use super::types::NetrcEntry;

#[test]
fn test_parse_basic_machine() {
    let content = "machine ftp.example.com\nlogin myuser\npassword mypass\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert_eq!(parser.len(), 1);
    assert!(!parser.is_empty());

    let entry = parser.find("ftp.example.com").expect("entry should exist");
    assert_eq!(entry.machine, "ftp.example.com");
    assert_eq!(entry.login.as_deref(), Some("myuser"));
    assert_eq!(entry.password.as_deref(), Some("mypass"));
    assert!(entry.account.is_none());
}

#[test]
fn test_parse_with_account() {
    let content = "machine host.example.com\nlogin user1\npassword pass1\naccount acct123\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    let entry = parser.find("host.example.com").expect("entry should exist");
    assert_eq!(entry.account.as_deref(), Some("acct123"));
}

#[test]
fn test_parse_default_entry() {
    let content = "default\nlogin anonymous\npassword guest@\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert!(parser.find("anything.example.com").is_none());
    let def = parser.find_default().expect("default should exist");
    assert!(def.is_default());
    assert_eq!(def.login.as_deref(), Some("anonymous"));
    assert_eq!(def.password.as_deref(), Some("guest@"));
}

#[test]
fn test_parse_machine_and_default() {
    let content = "\
        machine ftp.example.com\n\
        login myuser\n\
        password mypass\n\
        default\n\
        login anon\n\
        password guest@\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert_eq!(parser.len(), 2);

    // Exact match
    let entry = parser.find("ftp.example.com").expect("entry should exist");
    assert_eq!(entry.login.as_deref(), Some("myuser"));

    // Fallback to default
    let fallback = parser
        .find_with_fallback("unknown.host.com")
        .expect("fallback should exist");
    assert!(fallback.is_default());
    assert_eq!(fallback.login.as_deref(), Some("anon"));
}

#[test]
fn test_parse_multiple_machines() {
    let content = "\
        machine ftp.example.com\n\
        login user1\n\
        password pass1\n\
        \n\
        machine ssh.example.com\n\
        login user2\n\
        password pass2\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert_eq!(parser.entries().len(), 2);
    assert_eq!(
        parser
            .find("ftp.example.com")
            .expect("ftp entry")
            .login
            .as_deref(),
        Some("user1")
    );
    assert_eq!(
        parser
            .find("ssh.example.com")
            .expect("ssh entry")
            .login
            .as_deref(),
        Some("user2")
    );
}

#[test]
fn test_find_unknown_returns_none() {
    let content = "machine ftp.example.com\nlogin user\npassword pass\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert!(parser.find("unknown.com").is_none());
    assert!(parser.find_with_fallback("unknown.com").is_none());
}

#[test]
fn test_comments_and_blank_lines() {
    let content = "\
        # This is a comment\n\
        machine example.com\n\
        # Another comment\n\
        login user\n\
           password pass\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert_eq!(parser.len(), 1);
    let entry = &parser.entries()[0];
    assert_eq!(entry.login.as_deref(), Some("user"));
    assert_eq!(entry.password.as_deref(), Some("pass"));
}

#[test]
fn test_case_insensitive_keywords() {
    let content = "MACHINE example.com\nLOGIN user\nPASSWORD pass\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert_eq!(parser.entries()[0].login.as_deref(), Some("user"));
}

#[test]
fn test_passwd_alias() {
    let content = "machine example.com\nlogin user\npasswd secret\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert_eq!(parser.entries()[0].password.as_deref(), Some("secret"));
}

#[test]
fn test_inline_tokens() {
    // Tokens on a single line, matching C++ splitIterM behavior
    let content = "machine example.com login user password pass\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert_eq!(parser.len(), 1);
    let entry = &parser.entries()[0];
    assert_eq!(entry.machine, "example.com");
    assert_eq!(entry.login.as_deref(), Some("user"));
    assert_eq!(entry.password.as_deref(), Some("pass"));
}

#[test]
fn test_macdef_skipped() {
    let content = "\
        machine example.com\n\
        login user\n\
        password pass\n\
        macdef mymacro\n\
        this is macro body\n\
        \n\
        machine other.com\n\
        login otheruser\n\
        password otherpass\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert_eq!(parser.entries().len(), 2);
    assert_eq!(
        parser
            .find("example.com")
            .expect("example entry")
            .login
            .as_deref(),
        Some("user")
    );
    assert_eq!(
        parser
            .find("other.com")
            .expect("other entry")
            .login
            .as_deref(),
        Some("otheruser")
    );
}

#[test]
fn test_error_token_before_machine() {
    let content = "login user\n";
    let result = NetrcParser::parse(content);
    assert!(result.is_err());
    let err = result.unwrap_err();
    match err {
        NetrcError::ParseError(msg) => {
            assert!(msg.contains("'login'"));
            assert!(msg.contains("machine") || msg.contains("default"));
        }
        _ => panic!("expected ParseError, got {:?}", err),
    }
}

#[test]
fn test_error_premature_eof() {
    // "machine" keyword without a hostname value
    let content = "machine\n";
    let result = NetrcParser::parse(content);
    assert!(result.is_err());
    match result.unwrap_err() {
        NetrcError::ParseError(msg) => {
            assert!(msg.contains("EOF") || msg.contains("token"));
        }
        _ => panic!("expected ParseError"),
    }
}

#[test]
fn test_error_login_without_value() {
    let content = "machine example.com\nlogin\n";
    let result = NetrcParser::parse(content);
    assert!(result.is_err());
}

#[test]
fn test_empty_input() {
    let parser = NetrcParser::parse("").expect("empty input should parse");
    assert!(parser.is_empty());
    assert_eq!(parser.len(), 0);
}

#[test]
fn test_only_comments() {
    let content = "# just a comment\n# another comment\n";
    let parser = NetrcParser::parse(content).expect("comments only should parse");
    assert!(parser.is_empty());
}

#[test]
fn test_entry_has_credentials() {
    let mut entry = NetrcEntry::new("host".to_string());
    assert!(!entry.has_credentials());
    entry.login = Some("user".to_string());
    assert!(!entry.has_credentials());
    entry.password = Some("pass".to_string());
    assert!(entry.has_credentials());
}

#[test]
fn test_entry_is_default() {
    let machine_entry = NetrcEntry::new("host".to_string());
    assert!(!machine_entry.is_default());

    let default_entry = NetrcEntry::new_default();
    assert!(default_entry.is_default());
}

#[test]
fn test_error_display() {
    let err = NetrcError::FileNotFound("/missing/.netrc".into());
    assert!(err.to_string().contains("/missing/.netrc"));

    let err2 = NetrcError::ParseError("bad token".into());
    assert!(err2.to_string().contains("parse error"));

    let err3 = NetrcError::IoError("read failed".into());
    assert!(err3.to_string().contains("I/O"));
}

#[test]
fn test_parse_file_not_found() {
    let result = NetrcParser::parse_file(Path::new("/nonexistent/.netrc"));
    assert!(result.is_err());
    match result.unwrap_err() {
        NetrcError::FileNotFound(_) => {}
        other => panic!("expected FileNotFound, got {:?}", other),
    }
}

#[test]
fn test_convert_to_netrc_store() {
    let content = "\
        machine ftp.example.com\n\
        login myuser\n\
        password mypass\n\
        default\n\
        login anon\n\
        password guest@\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    let store: super::super::NetrcStore = parser.into();

    // Machine entry
    let entry = store
        .find("ftp.example.com")
        .expect("store entry should exist");
    assert_eq!(entry.login, "myuser");
    assert_eq!(entry.password, "mypass");

    // Default entry accessible via default_entry()
    let default = store.default_entry();
    assert!(default.is_some());
    assert_eq!(default.unwrap().login, "anon");

    // Fallback lookup
    let fallback = store
        .find_with_fallback("unknown.host.com")
        .expect("fallback should exist");
    assert_eq!(fallback.login, "anon");
}

#[test]
fn test_convert_skips_entries_without_credentials() {
    // Entry with only login, no password — should be skipped
    let content = "machine incomplete.com\nlogin user\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    let store: super::super::NetrcStore = parser.into();
    assert!(store.is_empty());
}

#[test]
fn test_real_world_netrc_format() {
    let content = "\
        # ~/.netrc\n\
        machine ftp.example.com\n\
        login myuser\n\
        password mypass\n\
        \n\
        machine api.example.com\n\
        login apiuser\n\
        password apipass\n\
        account prod\n\
        \n\
        default\n\
        login anonymous\n\
        password guest@\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");

    assert_eq!(parser.entries().len(), 2);
    assert!(parser.default_entry().is_some());

    // Exact lookup
    let ftp = parser.find("ftp.example.com").expect("ftp entry");
    assert_eq!(ftp.login.as_deref(), Some("myuser"));
    assert!(ftp.account.is_none());

    let api = parser.find("api.example.com").expect("api entry");
    assert_eq!(api.login.as_deref(), Some("apiuser"));
    assert_eq!(api.account.as_deref(), Some("prod"));

    // Fallback
    let fallback = parser.find_with_fallback("unknown.com").expect("fallback");
    assert_eq!(fallback.login.as_deref(), Some("anonymous"));
}

#[test]
fn test_tabs_as_whitespace() {
    let content = "machine\texample.com\tlogin\tuser\tpassword\tpass\n";
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    let entry = parser.find("example.com").expect("entry should exist");
    assert_eq!(entry.login.as_deref(), Some("user"));
    assert_eq!(entry.password.as_deref(), Some("pass"));
}

#[test]
fn test_mixed_whitespace_and_comments() {
    let content = "  machine   example.com   # inline comment\n  login   user\n  password   pass\n";
    // Note: inline comments after tokens are NOT standard netrc.
    // The C++ parser doesn't support them either — the "# inline" would
    // be parsed as extra tokens. Here we just verify the parser doesn't
    // crash and picks up the first tokens correctly.
    let parser = NetrcParser::parse(content).expect("parse should succeed");
    assert_eq!(parser.entries()[0].machine, "example.com");
}
