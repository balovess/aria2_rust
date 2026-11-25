//! Firefox / Chromium SQLite cookie store import.
//!
//! Port of C++ `Sqlite3CookieParser` + `Sqlite3CookieParserImpl`
//! (`Sqlite3MozCookieParser` / `Sqlite3ChromiumCookieParser`).
//!
//! Both browsers keep their cookie jar in a SQLite database — Firefox in
//! `cookies.sqlite` (table `moz_cookies`), Chromium in `Cookies` (table
//! `cookies`). Their schemas differ in column names and timestamp epochs. This
//! module reads only the target table and maps the relevant columns by name.
//!
//! # Known limitations (same as C++ aria2)
//!
//! - The database and optional `-wal` sidecar are read as byte snapshots. WAL
//!   commits are included, but a browser writing either file during the read can
//!   still cause a transient parse error; retry after the write completes.
//! - Chromium encrypts cookie values at rest (`encrypted_value`) on all modern
//!   versions; the plaintext `value` column is then empty. Such cookies import
//!   with an empty value. Decryption needs per-platform OS keyring access and is
//!   out of scope here, exactly as in C++ aria2.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use sqlite_core::{Database, Row, Value};
use tracing::debug;

use crate::error::{Aria2Error, Result};
use crate::http::cookie::parsing::is_numeric_host;
use crate::http::cookie::{Cookie, SameSite};

const CHROMIUM_EPOCH_OFFSET: i64 = 11_644_473_600;

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
        parse_cookie_table(path, CookieSchema::Firefox)
    }

    /// Parse a Chromium `Cookies` database.
    pub fn parse_chromium(path: &Path) -> Result<Vec<Cookie>> {
        parse_cookie_table(path, CookieSchema::Chromium)
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
}

#[derive(Clone, Copy)]
enum CookieSchema {
    Firefox,
    Chromium,
}

/// Read the main database and its optional WAL overlay, then decode only the
/// browser cookie table. A present but unreadable WAL is an error: ignoring it
/// could silently import stale cookies while the browser is running.
fn parse_cookie_table(path: &Path, schema: CookieSchema) -> Result<Vec<Cookie>> {
    let main = std::fs::read(path).map_err(|error| {
        Aria2Error::Io(format!(
            "Failed to read SQLite cookie database {}: {error}",
            path.display()
        ))
    })?;
    let wal_path = wal_path(path);
    let wal = match std::fs::read(&wal_path) {
        Ok(wal) if !wal.is_empty() => Some(wal),
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(Aria2Error::Io(format!(
                "Failed to read SQLite cookie WAL {}: {error}",
                wal_path.display()
            )));
        }
    };
    let database = match wal.as_deref() {
        Some(wal) => Database::open_with_wal(main, wal),
        None => Database::open(main),
    }
    .map_err(|error| Aria2Error::Parse(format!("Failed to read SQLite3 database: {error:?}")))?;

    let expected_table = match schema {
        CookieSchema::Firefox => "moz_cookies",
        CookieSchema::Chromium => "cookies",
    };
    let table = database
        .live_tables()
        .into_iter()
        .find(|table| table.name.eq_ignore_ascii_case(expected_table))
        .ok_or_else(|| {
            Aria2Error::Parse(format!(
                "Failed to read SQLite3 database: no such table: {expected_table}"
            ))
        })?;
    let columns = table.column_names.ok_or_else(|| {
        Aria2Error::Parse(format!(
            "Failed to read SQLite3 database: unable to read columns for {expected_table}"
        ))
    })?;
    let index = |name: &str| {
        columns
            .iter()
            .position(|column| column.eq_ignore_ascii_case(name))
    };
    let (host, path_col, secure, expiry, name, value, last_access) = match schema {
        CookieSchema::Firefox => (
            index("host"),
            index("path"),
            index("isSecure"),
            index("expiry"),
            index("name"),
            index("value"),
            index("lastAccessed"),
        ),
        CookieSchema::Chromium => (
            index("host_key"),
            index("path"),
            index("is_secure").or_else(|| index("secure")),
            index("expires_utc"),
            index("name"),
            index("value"),
            index("last_access_utc"),
        ),
    };
    let indices = [host, path_col, secure, expiry, name, value, last_access];
    if indices.iter().any(Option::is_none) {
        return Err(Aria2Error::Parse(format!(
            "Failed to read SQLite3 database: required columns missing from {expected_table}"
        )));
    }
    let indices = indices.map(Option::unwrap);
    let rows = database
        .read_table(table.rootpage, columns.len())
        .map_err(|error| {
            Aria2Error::Parse(format!("Failed to read SQLite3 database: {error:?}"))
        })?;
    Ok(rows
        .iter()
        .filter_map(|row| row_to_cookie(row, indices, matches!(schema, CookieSchema::Chromium)))
        .collect())
}

fn wal_path(path: &Path) -> PathBuf {
    let mut sidecar = OsString::from(path.as_os_str());
    sidecar.push("-wal");
    PathBuf::from(sidecar)
}

fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn value_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Integer(value) => Some(*value),
        Value::Real(value) if value.is_finite() => Some(*value as i64),
        Value::Text(value) => value.trim().parse().ok(),
        _ => None,
    }
}

/// Convert one database row into a [`Cookie`], or `None` if it is unusable.
fn row_to_cookie(row: &Row, indices: [usize; 7], chromium: bool) -> Option<Cookie> {
    // C++ rejects the row outright when host / path / name are NULL.
    let raw_host = value_text(row.values.get(indices[0])?)?;
    let path = value_text(row.values.get(indices[1])?)?;
    let name = value_text(row.values.get(indices[4])?)?;

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
    let mut expiry_time = value_i64(row.values.get(indices[3])?)?;
    let mut last_access_time = value_i64(row.values.get(indices[6])?)?;
    if chromium {
        expiry_time = (expiry_time / 1_000_000).checked_sub(CHROMIUM_EPOCH_OFFSET)?;
        last_access_time = (last_access_time / 1_000_000).checked_sub(CHROMIUM_EPOCH_OFFSET)?;
    }

    // NULL value is legal and means empty (C++ `toString(nullptr)` -> "").
    let value = row
        .values
        .get(indices[5])
        .and_then(value_text)
        .unwrap_or_default();

    // Numeric hosts can never have subdomains, so they are always host-only
    // regardless of the leading dot.
    let host_only = is_numeric_host(&domain) || !had_leading_dot;

    // C++ compares the stringified column against "1".
    let secure = row.values.get(indices[2]).and_then(value_i64) == Some(1);

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
    use tempfile::TempDir;

    const FIREFOX_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sqlite_cookie_parser/firefox.sqlite");
    const FIREFOX_STORAGE_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sqlite_cookie_parser/firefox_storage.sqlite");
    const FIREFOX_TEXT_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sqlite_cookie_parser/firefox_text.sqlite");
    const CHROMIUM_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sqlite_cookie_parser/chromium.sqlite");
    const CHROMIUM_LEGACY_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sqlite_cookie_parser/chromium_legacy.sqlite");
    const CHROMIUM_WAL_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sqlite_cookie_parser/chromium_wal.sqlite");
    const CHROMIUM_WAL: &[u8] =
        include_bytes!("../../tests/fixtures/sqlite_cookie_parser/chromium_wal.sqlite-wal");
    const UNKNOWN_FIXTURE: &[u8] =
        include_bytes!("../../tests/fixtures/sqlite_cookie_parser/unknown.sqlite");

    type CookieSignature = (String, String, String, String, i64, i64, bool);

    fn fixture(data: &[u8], filename: &str) -> (TempDir, PathBuf) {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(filename);
        std::fs::write(&path, data).unwrap();
        (directory, path)
    }

    fn signature(cookie: &Cookie) -> CookieSignature {
        (
            cookie.domain.clone(),
            cookie.path.clone(),
            cookie.name.clone(),
            cookie.value.clone(),
            cookie.expiry_time,
            cookie.last_access_time,
            cookie.secure,
        )
    }

    fn signatures(cookies: &[Cookie]) -> Vec<CookieSignature> {
        cookies.iter().map(signature).collect()
    }

    #[test]
    fn firefox_fixture_covers_schema_mapping_and_invalid_rows() {
        let (_directory, path) = fixture(FIREFOX_FIXTURE, "cookies.sqlite");
        let cookies = Sqlite3CookieParser::parse_firefox(&path).unwrap();
        let expected = vec![
            (
                "example.org".into(),
                "/".into(),
                "JSESSIONID".into(),
                "123456789".into(),
                1_500_000_000,
                1_400_000_000,
                false,
            ),
            (
                "example.org".into(),
                "/".into(),
                "hostonly".into(),
                "v".into(),
                100,
                50,
                true,
            ),
            (
                "192.168.0.1".into(),
                "/".into(),
                "numeric".into(),
                "v".into(),
                100,
                50,
                false,
            ),
            (
                "example.org".into(),
                "/".into(),
                "nullvalue".into(),
                "".into(),
                100,
                50,
                false,
            ),
            (
                "example.org".into(),
                "/".into(),
                "good".into(),
                "v".into(),
                100,
                50,
                false,
            ),
            (
                "moz.test".into(),
                "/".into(),
                "k".into(),
                "v".into(),
                100,
                50,
                false,
            ),
        ];
        assert_eq!(signatures(&cookies), expected);

        let jsession = cookies
            .iter()
            .find(|cookie| cookie.name == "JSESSIONID")
            .unwrap();
        assert_eq!(jsession.creation_time, 1_400_000_000);
        assert!(jsession.persistent);
        assert!(
            !jsession.host_only,
            "leading dot means subdomains are included"
        );
        assert!(
            cookies
                .iter()
                .find(|cookie| cookie.name == "hostonly")
                .unwrap()
                .host_only
        );
        assert!(
            cookies
                .iter()
                .find(|cookie| cookie.name == "numeric")
                .unwrap()
                .host_only
        );
        assert_eq!(cookies.len(), 6, "malformed rows should be skipped");
    }

    #[test]
    fn firefox_text_timestamps_are_accepted() {
        let (_directory, path) = fixture(FIREFOX_TEXT_FIXTURE, "cookies.sqlite");
        let cookies = Sqlite3CookieParser::parse_firefox(&path).unwrap();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].expiry_time, 999);
        assert_eq!(cookies[0].last_access_time, 888);
        assert!(cookies[0].secure);
    }

    #[test]
    fn chromium_is_secure_fixture_matches_expected_values_and_epoch() {
        let (_directory, path) = fixture(CHROMIUM_FIXTURE, "Cookies");
        let cookies = Sqlite3CookieParser::parse_chromium(&path).unwrap();
        assert_eq!(
            signatures(&cookies),
            vec![
                (
                    "example.org".into(),
                    "/".into(),
                    "SID".into(),
                    "abc".into(),
                    1_355_526_400,
                    1_255_526_400,
                    true
                ),
                (
                    "example.net".into(),
                    "/app".into(),
                    "empty".into(),
                    "".into(),
                    1_455_526_400,
                    1_305_526_400,
                    false
                ),
            ]
        );
        assert!(cookies[0].host_only);
        assert_eq!(cookies[0].creation_time, 1_255_526_400);
    }

    #[test]
    fn chromium_legacy_secure_column_remains_supported() {
        let (_directory, path) = fixture(CHROMIUM_LEGACY_FIXTURE, "Cookies");
        let cookies = Sqlite3CookieParser::parse_chromium(&path).unwrap();
        assert_eq!(cookies.len(), 1);
        assert_eq!(cookies[0].name, "legacy");
        assert!(cookies[0].secure);
    }

    #[test]
    fn chromium_wal_fixture_reads_committed_rows_without_mutating_main_file() {
        let (_directory, path) = fixture(CHROMIUM_WAL_FIXTURE, "Cookies");
        let main_before = std::fs::read(&path).unwrap();
        let wal_file = wal_path(&path);
        std::fs::write(&wal_file, CHROMIUM_WAL).unwrap();
        assert!(std::fs::metadata(&wal_file).unwrap().len() > 32);

        let cookies = Sqlite3CookieParser::parse_chromium(&path).unwrap();
        assert_eq!(
            signatures(&cookies),
            vec![
                (
                    "base.example".into(),
                    "/".into(),
                    "base".into(),
                    "1".into(),
                    1_355_526_400,
                    1_255_526_400,
                    false
                ),
                (
                    "wal.example".into(),
                    "/".into(),
                    "from_wal".into(),
                    "2".into(),
                    1_455_526_400,
                    1_305_526_400,
                    true
                ),
            ]
        );
        assert_eq!(std::fs::read(&path).unwrap(), main_before);
    }

    #[test]
    fn parse_auto_detects_firefox_and_falls_back_to_chromium() {
        let (_firefox_directory, firefox_path) = fixture(FIREFOX_FIXTURE, "cookies.sqlite");
        let firefox = Sqlite3CookieParser::parse_auto(&firefox_path).unwrap();
        assert!(firefox.iter().any(|cookie| cookie.domain == "moz.test"));

        let (_chromium_directory, chromium_path) = fixture(CHROMIUM_FIXTURE, "Cookies");
        let chromium = Sqlite3CookieParser::parse_auto(&chromium_path).unwrap();
        assert_eq!(chromium[0].domain, "example.org");
    }

    #[test]
    fn parse_auto_reports_error_for_unknown_schema() {
        let (_directory, path) = fixture(UNKNOWN_FIXTURE, "random.db");
        let error = Sqlite3CookieParser::parse_auto(&path).unwrap_err();
        assert!(
            error.to_string().contains("moz_cookies"),
            "expected the Firefox error to be surfaced, got: {error}"
        );
    }

    #[test]
    fn missing_file_is_an_error_not_a_panic() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join("does-not-exist.sqlite");
        assert!(Sqlite3CookieParser::parse_firefox(&path).is_err());
    }

    #[test]
    fn cookie_storage_load_file_detects_sqlite_by_magic() {
        use crate::http::cookie::CookieStorage;

        let (_directory, path) = fixture(FIREFOX_STORAGE_FIXTURE, "cookies.sqlite");
        let storage = CookieStorage::new();
        assert_eq!(storage.load_file(&path).unwrap(), 2);
        assert_eq!(storage.count(), 2);
    }

    #[test]
    fn cookie_storage_load_file_still_reads_netscape() {
        use crate::http::cookie::CookieStorage;

        let directory = TempDir::new().unwrap();
        let path = directory.path().join("cookies.txt");
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
}
