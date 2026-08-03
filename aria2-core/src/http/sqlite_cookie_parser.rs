//! Firefox / Chromium SQLite cookie store import.
//!
//! Port of C++ `Sqlite3CookieParser` + `Sqlite3CookieParserImpl`
//! (`Sqlite3MozCookieParser` / `Sqlite3ChromiumCookieParser`).
//!
//! Both browsers keep their cookie jar in a SQLite database — Firefox in
//! `cookies.sqlite` (table `moz_cookies`), Chromium in `Cookies` (table
//! `cookies`). The two schemas differ in column names and in the epoch used
//! for timestamps, so each gets its own SELECT; everything downstream of the
//! query is shared.
//!
//! # Known limitations (same as C++ aria2)
//!
//! - The database is opened **read-only**. If the browser is running and holds
//!   an exclusive lock (typical for Chromium on Windows), the open fails — close
//!   the browser first.
//! - Chromium encrypts cookie values at rest (`encrypted_value`) on all modern
//!   versions; the plaintext `value` column is then empty. Such cookies import
//!   with an empty value. Decryption needs per-platform OS keyring access and is
//!   out of scope here, exactly as in C++ aria2.

use std::path::Path;

use rusqlite::types::ValueRef;
use rusqlite::{Connection, OpenFlags, Row};
use tracing::debug;

use crate::error::{Aria2Error, Result};
use crate::http::cookie::parsing::is_numeric_host;
use crate::http::cookie::{Cookie, SameSite};

/// Firefox `cookies.sqlite` query.
///
/// Column order is load-bearing — [`row_to_cookie`] indexes positionally, and
/// both browser queries must project the same seven columns in the same order.
pub const MOZ_QUERY: &str = "SELECT host, path, isSecure, expiry, name, value, lastAccessed \
     FROM moz_cookies";

/// Chromium `Cookies` query.
///
/// Chromium timestamps are microseconds since 1601-01-01 UTC (the Windows FILE
/// epoch). 11644473600 is the number of seconds between that epoch and the UNIX
/// epoch, so the arithmetic below normalises both time columns to UNIX seconds.
/// Done in SQL rather than in Rust to keep [`row_to_cookie`] schema-agnostic.
pub const CHROMIUM_QUERY: &str = "SELECT host_key, path, secure, expires_utc / 1000000 - 11644473600 as expires_utc, \
     name, value, last_access_utc / 1000000 - 11644473600 as last_access_utc \
     FROM cookies";

/// The 16-byte magic header every SQLite 3 database file starts with.
///
/// Used by [`is_sqlite_file`] to pick a parser without relying on the filename,
/// matching C++ `CookieStorage::load()`.
pub const SQLITE_MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Returns `true` if `data` begins with the SQLite 3 file magic.
pub fn is_sqlite_file(data: &[u8]) -> bool {
    data.len() >= SQLITE_MAGIC.len() && &data[..SQLITE_MAGIC.len()] == SQLITE_MAGIC.as_slice()
}

/// Reads Firefox / Chromium cookie databases.
pub struct Sqlite3CookieParser;

impl Sqlite3CookieParser {
    /// Parse a Firefox `cookies.sqlite` database.
    pub fn parse_firefox(path: &Path) -> Result<Vec<Cookie>> {
        Self::parse_with_query(path, MOZ_QUERY)
    }

    /// Parse a Chromium `Cookies` database.
    pub fn parse_chromium(path: &Path) -> Result<Vec<Cookie>> {
        Self::parse_with_query(path, CHROMIUM_QUERY)
    }

    /// Try Firefox first, fall back to Chromium.
    ///
    /// Mirrors C++ `CookieStorage::load()`: the file magic tells us it is a
    /// SQLite database but not which browser wrote it, so the schema is probed
    /// by attempting the Firefox query and retrying with the Chromium one when
    /// it fails (the failure is `no such table: moz_cookies`).
    ///
    /// If both fail, the **Firefox** error is returned — the Chromium attempt is
    /// the fallback, so its error is the less informative of the two when the
    /// file is neither (e.g. a random SQLite database).
    pub fn parse_auto(path: &Path) -> Result<Vec<Cookie>> {
        match Self::parse_firefox(path) {
            Ok(cookies) => Ok(cookies),
            Err(moz_err) => {
                debug!(
                    error = %moz_err,
                    "Not a Firefox cookie database, retrying as Chromium"
                );
                Self::parse_chromium(path).map_err(|chromium_err| {
                    debug!(error = %chromium_err, "Chromium cookie parse also failed");
                    moz_err
                })
            }
        }
    }

    /// Open `path` read-only and map every row of `query` to a [`Cookie`].
    ///
    /// Rows that fail validation are skipped rather than aborting the import,
    /// matching C++ `cookieRowMapper`, which returns 0 (continue) for bad rows.
    /// A real browser jar routinely contains entries we cannot represent, and
    /// dropping the whole file over one of them would be worse than useless.
    fn parse_with_query(path: &Path, query: &str) -> Result<Vec<Cookie>> {
        // Read-only, and deliberately without `SQLITE_OPEN_URI`: a path
        // containing `?` must be treated as a literal filename, not a URI with
        // query parameters. C++ passes bare `SQLITE_OPEN_READONLY` for the same
        // reason.
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(path, flags).map_err(|e| {
            Aria2Error::Io(format!(
                "Failed to open SQLite cookie database {}: {e}",
                path.display()
            ))
        })?;

        let mut stmt = conn
            .prepare(query)
            .map_err(|e| Aria2Error::Parse(format!("Failed to read SQLite3 database: {e}")))?;

        let mut rows = stmt
            .query([])
            .map_err(|e| Aria2Error::Parse(format!("Failed to read SQLite3 database: {e}")))?;

        let mut cookies = Vec::new();
        loop {
            let row = rows
                .next()
                .map_err(|e| Aria2Error::Parse(format!("Failed to read SQLite3 database: {e}")))?;
            match row {
                Some(row) => {
                    if let Some(cookie) = row_to_cookie(row) {
                        cookies.push(cookie);
                    }
                }
                None => break,
            }
        }
        Ok(cookies)
    }
}

/// Read a column as text, treating NULL and non-text values as absent.
///
/// C++ receives every column as `char*` from `sqlite3_exec`, so an INTEGER
/// `host` would arrive stringified. In practice these columns are always TEXT;
/// coercing integers here would only invent domains out of numbers.
fn column_text(row: &Row<'_>, idx: usize) -> Option<String> {
    match row.get_ref(idx).ok()? {
        ValueRef::Text(bytes) => Some(String::from_utf8_lossy(bytes).into_owned()),
        _ => None,
    }
}

/// Read a column as an integer, accepting both INTEGER and TEXT storage.
///
/// C++ stringifies via `sqlite3_exec` and then runs `parseLLIntNoThrow`, so a
/// timestamp stored as TEXT parses fine there. Accepting both keeps behaviour
/// identical for hand-edited or migrated databases. REAL is accepted and
/// truncated because Chromium's SQL division can yield a float when the source
/// column has REAL affinity.
fn column_i64(row: &Row<'_>, idx: usize) -> Option<i64> {
    match row.get_ref(idx).ok()? {
        ValueRef::Integer(v) => Some(v),
        ValueRef::Real(v) if v.is_finite() => Some(v as i64),
        ValueRef::Text(bytes) => std::str::from_utf8(bytes).ok()?.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// Convert one result row into a [`Cookie`], or `None` if it is unusable.
///
/// Port of C++ `cookieRowMapper`. Column order (shared by both queries):
/// `0 host, 1 path, 2 secure, 3 expiry, 4 name, 5 value, 6 last_access`.
fn row_to_cookie(row: &Row<'_>) -> Option<Cookie> {
    // C++ rejects the row outright when host / path / name are NULL.
    let raw_host = column_text(row, 0)?;
    let path = column_text(row, 1)?;
    let name = column_text(row, 4)?;

    // A leading dot is the legacy "include subdomains" marker; the stored
    // domain never keeps it, but whether it was present decides host_only below.
    let had_leading_dot = raw_host.starts_with('.');
    let domain = raw_host.trim_start_matches('.').to_string();

    // C++ `goodPath`: non-empty and rooted at '/'.
    if name.is_empty() || domain.is_empty() || !path.starts_with('/') {
        return None;
    }

    // A row with an unparseable timestamp is dropped rather than defaulted —
    // guessing an expiry could resurrect a cookie the user expected to lapse.
    let expiry_time = column_i64(row, 3)?;
    let last_access_time = column_i64(row, 6)?;

    // NULL value is legal and means empty (C++ `toString(nullptr)` -> "").
    let value = column_text(row, 5).unwrap_or_default();

    // Numeric hosts can never have subdomains, so they are always host-only
    // regardless of the leading dot.
    let host_only = is_numeric_host(&domain) || !had_leading_dot;

    // C++ compares the stringified column against "1".
    let secure = column_i64(row, 2) == Some(1);

    Some(Cookie {
        name,
        value,
        domain,
        path,
        expiry_time,
        // C++ passes lastAccessed as the creation time (the schema has no
        // creation column that both browsers agree on).
        creation_time: last_access_time,
        last_access_time,
        // Anything persisted to the browser's on-disk jar is by definition a
        // persistent cookie.
        persistent: true,
        host_only,
        secure,
        // Neither query projects the httpOnly column; C++ hardcodes false.
        http_only: false,
        same_site: SameSite::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Build a Firefox-shaped database and return its path (the `TempDir` must
    /// outlive it, hence returning both).
    fn make_moz_db(
        rows: &[(&str, &str, i64, i64, &str, Option<&str>, i64)],
    ) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cookies.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE moz_cookies (
                 host TEXT, path TEXT, isSecure INTEGER, expiry INTEGER,
                 name TEXT, value TEXT, lastAccessed INTEGER)",
            [],
        )
        .unwrap();
        for (host, path_col, secure, expiry, name, value, last) in rows {
            conn.execute(
                "INSERT INTO moz_cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![host, path_col, secure, expiry, name, value, last],
            )
            .unwrap();
        }
        drop(conn);
        (dir, path)
    }

    fn make_chromium_db(
        rows: &[(&str, &str, i64, i64, &str, &str, i64)],
    ) -> (TempDir, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("Cookies");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE cookies (
                 host_key TEXT, path TEXT, secure INTEGER, expires_utc INTEGER,
                 name TEXT, value TEXT, last_access_utc INTEGER)",
            [],
        )
        .unwrap();
        for (host, path_col, secure, expires, name, value, last) in rows {
            conn.execute(
                "INSERT INTO cookies VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![host, path_col, secure, expires, name, value, last],
            )
            .unwrap();
        }
        drop(conn);
        (dir, path)
    }

    #[test]
    fn parses_firefox_database() {
        let (_dir, path) = make_moz_db(&[(
            ".example.org",
            "/",
            0,
            1_500_000_000,
            "JSESSIONID",
            Some("123456789"),
            1_400_000_000,
        )]);
        let cookies = Sqlite3CookieParser::parse_firefox(&path).unwrap();
        assert_eq!(cookies.len(), 1);
        let c = &cookies[0];
        assert_eq!(c.name, "JSESSIONID");
        assert_eq!(c.value, "123456789");
        assert_eq!(c.domain, "example.org", "leading dot must be stripped");
        assert_eq!(c.path, "/");
        assert_eq!(c.expiry_time, 1_500_000_000);
        assert_eq!(c.last_access_time, 1_400_000_000);
        assert_eq!(
            c.creation_time, 1_400_000_000,
            "C++ passes lastAccessed as creation time"
        );
        assert!(c.persistent);
        assert!(!c.secure);
        assert!(
            !c.host_only,
            "leading dot means the cookie spans subdomains"
        );
    }

    #[test]
    fn host_without_leading_dot_is_host_only() {
        let (_dir, path) = make_moz_db(&[("example.org", "/", 1, 100, "k", Some("v"), 50)]);
        let cookies = Sqlite3CookieParser::parse_firefox(&path).unwrap();
        assert!(cookies[0].host_only);
        assert!(cookies[0].secure, "isSecure=1 must map to secure");
    }

    #[test]
    fn numeric_host_is_always_host_only() {
        // Leading dot present, but an IP literal can have no subdomains.
        let (_dir, path) = make_moz_db(&[(".192.168.0.1", "/", 0, 100, "k", Some("v"), 50)]);
        let cookies = Sqlite3CookieParser::parse_firefox(&path).unwrap();
        assert_eq!(cookies[0].domain, "192.168.0.1");
        assert!(cookies[0].host_only);
    }

    #[test]
    fn null_value_becomes_empty_string() {
        let (_dir, path) = make_moz_db(&[(".example.org", "/", 0, 100, "k", None, 50)]);
        let cookies = Sqlite3CookieParser::parse_firefox(&path).unwrap();
        assert_eq!(cookies[0].value, "");
    }

    /// Bad rows are skipped, not fatal — one malformed entry must not discard
    /// an otherwise valid browser jar.
    #[test]
    fn invalid_rows_are_skipped_without_failing_the_import() {
        let (_dir, path) = make_moz_db(&[
            ("", "/", 0, 100, "empty-host", Some("v"), 50),
            (
                ".example.org",
                "relative",
                0,
                100,
                "bad-path",
                Some("v"),
                50,
            ),
            (".example.org", "/", 0, 100, "", Some("v"), 50),
            (".example.org", "/", 0, 100, "good", Some("v"), 50),
        ]);
        let cookies = Sqlite3CookieParser::parse_firefox(&path).unwrap();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "good");
    }

    #[test]
    fn parses_chromium_database_and_converts_epoch() {
        // Chromium stores microseconds since 1601-01-01. 13e15 µs = 13e9 s since
        // 1601, i.e. 13_000_000_000 - 11_644_473_600 = 1_355_526_400 UNIX seconds
        // (2012-12-15) — a plausible value, which is the point: an off-by-1000
        // conversion bug would land far outside the representable date range and
        // has to be caught here rather than by a user with an expired jar.
        let (_dir, path) = make_chromium_db(&[(
            "example.com",
            "/app",
            1,
            13_000_000_000_000_000,
            "SID",
            "abc",
            12_900_000_000_000_000,
        )]);
        let cookies = Sqlite3CookieParser::parse_chromium(&path).unwrap();
        assert_eq!(cookies.len(), 1);
        let c = &cookies[0];
        assert_eq!(c.name, "SID");
        assert_eq!(c.domain, "example.com");
        assert_eq!(c.path, "/app");
        assert!(c.secure);
        assert!(c.host_only, "no leading dot -> host only");
        assert_eq!(c.expiry_time, 1_355_526_400);
        assert_eq!(c.last_access_time, 1_255_526_400);
        assert_eq!(
            c.creation_time, 1_255_526_400,
            "C++ passes last_access_utc as the creation time"
        );
    }

    #[test]
    fn parse_auto_detects_firefox() {
        let (_dir, path) = make_moz_db(&[(".moz.test", "/", 0, 100, "k", Some("v"), 50)]);
        let cookies = Sqlite3CookieParser::parse_auto(&path).unwrap();
        assert_eq!(cookies[0].domain, "moz.test");
    }

    #[test]
    fn parse_auto_falls_back_to_chromium() {
        let (_dir, path) =
            make_chromium_db(&[("chrome.test", "/", 0, 13_000_000_000_000_000, "k", "v", 0)]);
        let cookies = Sqlite3CookieParser::parse_auto(&path).unwrap();
        assert_eq!(cookies[0].domain, "chrome.test");
    }

    /// A SQLite file with neither schema must surface the Firefox error, which
    /// is the more useful of the two.
    #[test]
    fn parse_auto_reports_error_for_unknown_schema() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("random.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute("CREATE TABLE unrelated (a INTEGER)", [])
            .unwrap();
        drop(conn);

        let err = Sqlite3CookieParser::parse_auto(&path).unwrap_err();
        assert!(
            err.to_string().contains("moz_cookies"),
            "expected the Firefox error to be surfaced, got: {err}"
        );
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("does-not-exist.sqlite");
        assert!(Sqlite3CookieParser::parse_firefox(&path).is_err());
    }

    /// End-to-end: `CookieStorage::load_file` must route by file magic, not by
    /// filename — Chromium's jar is literally named `Cookies` with no extension.
    #[test]
    fn cookie_storage_load_file_detects_sqlite_by_magic() {
        use crate::http::cookie::CookieStorage;

        let (_dir, path) = make_moz_db(&[
            (".example.org", "/", 0, 4_000_000_000, "a", Some("1"), 50),
            (".example.net", "/", 0, 4_000_000_000, "b", Some("2"), 50),
        ]);

        let storage = CookieStorage::new();
        let n = storage.load_file(&path).unwrap();
        assert_eq!(n, 2);
        assert_eq!(storage.count(), 2);
    }

    /// The Netscape path must keep working unchanged — switching `load_file`
    /// from `read_to_string` to `read` + lossy conversion must not regress it.
    #[test]
    fn cookie_storage_load_file_still_reads_netscape() {
        use crate::http::cookie::CookieStorage;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cookies.txt");
        std::fs::write(
            &path,
            "# Netscape HTTP Cookie File\n\
             .example.org\tTRUE\t/\tFALSE\t4000000000\tk\tv\n",
        )
        .unwrap();

        let storage = CookieStorage::new();
        assert_eq!(storage.load_file(&path).unwrap(), 1);
    }

    #[test]
    fn detects_sqlite_magic() {
        assert!(is_sqlite_file(b"SQLite format 3\0extra bytes"));
        assert!(!is_sqlite_file(b"SQLite format 3"), "15 bytes is too short");
        assert!(!is_sqlite_file(b"# Netscape HTTP Cookie File"));
        assert!(!is_sqlite_file(b""));
    }

    /// Real browser databases store timestamps as INTEGER, but hand-migrated
    /// ones sometimes use TEXT. C++ parses the stringified column either way.
    #[test]
    fn text_timestamps_are_accepted() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("cookies.sqlite");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "CREATE TABLE moz_cookies (
                 host TEXT, path TEXT, isSecure TEXT, expiry TEXT,
                 name TEXT, value TEXT, lastAccessed TEXT)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO moz_cookies VALUES ('.t.example', '/', '1', '999', 'k', 'v', '888')",
            [],
        )
        .unwrap();
        drop(conn);

        let cookies = Sqlite3CookieParser::parse_firefox(&path).unwrap();
        assert_eq!(cookies[0].expiry_time, 999);
        assert_eq!(cookies[0].last_access_time, 888);
        assert!(cookies[0].secure);
    }
}
