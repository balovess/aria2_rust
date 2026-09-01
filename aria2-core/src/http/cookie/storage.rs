//! Thread-safe cookie storage using RwLock for concurrent access.
//!
//! This module provides `CookieStorage`, the original cookie storage that works
//! with the `Cookie` struct from the parent module, providing add/find/expire
//! operations with host+path matching per RFC 6265.
//!
//! # Domain eviction
//!
//! Per C++ aria2 `CookieStorage`, when the number of tracked domain nodes
//! reaches `DOMAIN_EVICTION_TRIGGER` (2000), the least-recently-accessed
//! domains are evicted at a rate of `DOMAIN_EVICTION_RATE` (10%). Each domain
//! can hold at most `MAX_COOKIE_PER_DOMAIN` (50) cookies; if exceeded, expired
//! cookies are purged first, then the LRU cookie is replaced.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Aria2Error, Result};
use crate::http::ns_cookie_parser::NsCookieParser;
use crate::http::sqlite_cookie_parser::{Sqlite3CookieParser, is_sqlite_file};

use super::Cookie;

/// Number of tracked domain nodes that triggers eviction.
/// Matches C++ aria2 `DOMAIN_EVICTION_TRIGGER`.
pub const DOMAIN_EVICTION_TRIGGER: usize = 2000;

/// Fraction of domains to evict when the trigger is reached.
/// Matches C++ aria2 `DOMAIN_EVICTION_RATE`.
pub const DOMAIN_EVICTION_RATE: f64 = 0.1;

/// Cookies for a single domain.
///
/// In C++ aria2, each `DomainNode` holds a `deque<unique_ptr<Cookie>>` and
/// tracks `lastAccessTime_` / `lruAccessTime_`. This struct is the Rust
/// equivalent: a domain bucket with its cookies. The LRU timestamp is tracked
/// externally in `CookieStorage::lru_tracker` (matching C++ where the
/// `lruTracker_` set stores `(time_t, DomainNode*)` pairs).
#[derive(Debug)]
struct DomainBucket {
    cookies: Vec<Cookie>,
}

impl DomainBucket {
    fn new() -> Self {
        Self {
            cookies: Vec::new(),
        }
    }
}

/// Thread-safe cookie storage using RwLock for concurrent access.
///
/// Uses a `HashMap<Arc<str>, DomainBucket>` keyed by domain name, plus a
/// `BTreeSet<(seq, Arc<str>)>` tracker for efficient domain-level LRU eviction.
/// This mirrors the C++ aria2 domain-tree + LRU tracker design without
/// requiring raw-pointer tree navigation.
///
/// A monotonic sequence counter (`lru_seq`) replaces the C++ `time_t`
/// key in the LRU set, ensuring that every `update_lru` call produces
/// a unique and strictly increasing key — even when multiple updates
/// happen within the same second.
pub struct CookieStorage {
    /// Domain → bucket of cookies. Replaces the C++ DomainNode tree;
    /// the HashMap gives O(1) domain lookup instead of O(depth) tree traversal.
    domains: RwLock<HashMap<Arc<str>, DomainBucket>>,
    /// LRU tracker: sorted by (seq, domain) so the least-recently-accessed
    /// domain is always at the front. Replaces the C++ `std::set<pair<time_t, DomainNode*>>`.
    /// A monotonically-increasing sequence number is used instead of `time_t`
    /// to guarantee strict ordering even for operations within the same second.
    lru_tracker: RwLock<BTreeSet<(u64, Arc<str>)>>,
    /// Monotonic sequence counter for LRU tracker keys.
    lru_seq: AtomicU64,
}

static SHARED_COOKIE_STORAGE: OnceLock<Arc<CookieStorage>> = OnceLock::new();

impl CookieStorage {
    /// Normalize a cookie-domain key once at the storage boundary.
    ///
    /// DNS names are case-insensitive and a leading/trailing dot is only
    /// syntax for a domain cookie. Keeping one canonical key prevents
    /// duplicate buckets and lets lookup use the same representation for
    /// parsed cookies and manually supplied cookies.
    fn canonical_domain(domain: &str) -> String {
        domain
            .trim()
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_ascii_lowercase()
    }

    /// Return the only domain buckets that can contain cookies for `host`.
    ///
    /// This is the HashMap equivalent of aria2's domain tree walk: a request
    /// for `a.b.example.test` can match `a.b.example.test`, `b.example.test`,
    /// and `example.test`, but never an unrelated domain. IP literals do not
    /// have parent domains.
    fn domain_candidates(host: &str) -> Vec<String> {
        let host = Self::canonical_domain(host);
        if host.is_empty() {
            return Vec::new();
        }
        if host.parse::<std::net::IpAddr>().is_ok() {
            return vec![host];
        }

        let mut candidates = Vec::new();
        let mut start = 0;
        loop {
            let candidate = &host[start..];
            candidates.push(candidate.to_string());
            let Some(dot) = candidate.find('.') else {
                break;
            };
            start += dot + 1;
        }
        candidates
    }

    pub fn shared() -> Arc<Self> {
        Arc::clone(SHARED_COOKIE_STORAGE.get_or_init(|| Arc::new(Self::new())))
    }

    pub fn new() -> Self {
        Self {
            domains: RwLock::new(HashMap::new()),
            lru_tracker: RwLock::new(BTreeSet::new()),
            lru_seq: AtomicU64::new(1),
        }
    }

    /// Allocate the next LRU sequence number (monotonically increasing).
    fn next_lru_seq(&self) -> u64 {
        self.lru_seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Add a cookie to storage. If a cookie with the same name+domain+path
    /// already exists, it is replaced (preserving creation_time per RFC 6265).
    ///
    /// If the cookie is a "delete cookie" (Max-Age <= 0 or already expired),
    /// the existing cookie is removed instead.
    ///
    /// Enforces two levels of limits (matching C++ aria2):
    /// 1. Per-domain: `MAX_COOKIE_PER_DOMAIN` (50) — evict expired then LRU
    /// 2. Global: `DOMAIN_EVICTION_TRIGGER` (2000) — evict 10% of domains by LRU
    pub fn add(&self, cookie: Cookie) {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mut cookie = cookie;
        cookie.domain = Self::canonical_domain(&cookie.domain);
        if cookie.domain.is_empty() {
            return;
        }

        // Check domain eviction trigger before adding
        self.evict_domains_if_needed();

        let domain = cookie.domain.clone();
        let mut domains = self.domains.write().unwrap_or_else(|e| e.into_inner());

        // Ensure the domain bucket exists
        if !domains.contains_key(domain.as_str()) {
            domains.insert(Arc::from(domain.as_str()), DomainBucket::new());
        }
        let domain_key = domains
            .get_key_value(domain.as_str())
            .map(|(key, _)| Arc::clone(key))
            .expect("domain bucket must exist after insertion");
        let bucket = domains.get_mut(&domain_key).unwrap();

        // Check for existing cookie with same name+domain+path
        if let Some(pos) = bucket.cookies.iter().position(|c| c == &cookie) {
            if cookie.is_delete_cookie() {
                // Delete cookie: remove the existing entry
                bucket.cookies.remove(pos);
                // If bucket is now empty, remove from LRU tracker
                if bucket.cookies.is_empty() {
                    self.remove_from_lru(&domain_key);
                    domains.remove(&domain_key);
                } else {
                    self.update_lru(Arc::clone(&domain_key));
                }
                return;
            }
            // Preserve creation time from the existing cookie per RFC 6265 Section 5.3
            let mut updated = cookie;
            updated.creation_time = bucket.cookies[pos].creation_time;
            bucket.cookies[pos] = updated;
            self.update_lru(Arc::clone(&domain_key));
            return;
        }

        // If this is a delete cookie with no existing match, just skip it
        if cookie.is_delete_cookie() {
            // Remove empty bucket if we just created one
            if bucket.cookies.is_empty() {
                self.remove_from_lru(&domain_key);
                domains.remove(&domain_key);
            }
            return;
        }

        // Enforce max cookies per domain (matches C++ DomainNode::addCookie)
        if bucket.cookies.len() >= Cookie::max_cookie_per_domain() {
            // First try to evict expired cookies for this domain
            let before = bucket.cookies.len();
            bucket.cookies.retain(|c| !c.is_expired(now));
            if bucket.cookies.len() == before {
                // No expired cookies; replace least-recently-accessed cookie.
                // Per C++: `*m = std::move(cookie)` — the LRU cookie is replaced
                // (not just removed), so the new cookie takes its slot.
                if let Some(lru_pos) = bucket
                    .cookies
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, c)| c.last_access_time)
                    .map(|(i, _)| i)
                {
                    bucket.cookies[lru_pos] = cookie;
                    self.update_lru(Arc::clone(&domain_key));
                    return;
                }
            }
            // After eviction, we have room — push below
        }

        bucket.cookies.push(cookie);
        self.update_lru(domain_key);
    }

    /// Evict domains when the LRU tracker size reaches DOMAIN_EVICTION_TRIGGER.
    ///
    /// Per C++ `CookieStorage::store()`: when `lruTracker_.size() >=
    /// DOMAIN_EVICTION_TRIGGER`, evict `size * DOMAIN_EVICTION_RATE` domains,
    /// removing the least-recently-accessed ones first.
    fn evict_domains_if_needed(&self) {
        let lru = self.lru_tracker.read().unwrap_or_else(|e| e.into_inner());
        if lru.len() < DOMAIN_EVICTION_TRIGGER {
            return;
        }
        drop(lru);

        // Calculate how many domains to evict
        let lru = self.lru_tracker.read().unwrap_or_else(|e| e.into_inner());
        let delnum = (lru.len() as f64 * DOMAIN_EVICTION_RATE) as usize;
        drop(lru);

        if delnum == 0 {
            return;
        }

        self.evict_domains(delnum);
    }

    /// Evict `delnum` least-recently-accessed domains.
    ///
    /// Per C++ `CookieStorage::evictNode()`: iterate the LRU tracker from
    /// the front (oldest first), clear each domain's cookies, and remove
    /// the domain node. In our HashMap design, we simply remove the entry.
    fn evict_domains(&self, delnum: usize) {
        let mut domains = self.domains.write().unwrap_or_else(|e| e.into_inner());
        // Keep the lock order consistent with add(), expire_cookies(), and
        // clear(): domains -> lru. The previous inverse order could deadlock
        // when an insertion crossed the global eviction threshold.
        let mut lru = self.lru_tracker.write().unwrap_or_else(|e| e.into_inner());

        let mut evicted = 0;
        while evicted < delnum {
            let front = match lru.iter().next().cloned() {
                Some(entry) => entry,
                None => break,
            };
            lru.remove(&front);
            let domain = &front.1;
            domains.remove(domain);
            evicted += 1;
        }
    }

    /// Update the LRU tracker for a domain: remove the old entry (if any)
    /// and insert a new one with a fresh sequence number.
    ///
    /// Per C++ `CookieStorage::updateLru()`. The sequence counter ensures
    /// strict ordering even for rapid successive updates within the same
    /// wall-clock second, which is critical for correct LRU eviction.
    fn update_lru(&self, domain: Arc<str>) {
        let seq = self.next_lru_seq();
        let mut lru = self.lru_tracker.write().unwrap_or_else(|e| e.into_inner());
        // Remove old entry by scanning for matching domain.
        // (In C++, the node stores its own lruAccessTime for O(log n) removal;
        // we do a linear scan here which is fine for typical domain counts.)
        if let Some(old) = lru
            .iter()
            .find(|(_, d)| d.as_ref() == domain.as_ref())
            .cloned()
        {
            lru.remove(&old);
        }
        lru.insert((seq, domain));
    }

    /// Remove a domain from the LRU tracker.
    fn remove_from_lru(&self, domain: &str) {
        let mut lru = self.lru_tracker.write().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = lru
            .iter()
            .find(|(_, d)| d.as_ref() == domain)
            .cloned()
        {
            lru.remove(&old);
        }
    }

    /// Find all cookies matching the given host, path, and security context.
    ///
    /// Per RFC 6265 Section 5.4, cookies are sorted by:
    /// 1. Path depth descending (longer/more specific paths first)
    /// 2. Creation time ascending (earlier-created cookies first)
    ///
    /// Matching cookies have their `last_access_time` updated, and the
    /// domain's LRU timestamp is refreshed (matching C++ `criteriaFind`
    /// which calls `updateLru` on accessed domain nodes).
    pub fn find_cookies(
        &self,
        host: &str,
        path: &str,
        secure: bool,
        is_cross_site: bool,
    ) -> Vec<Cookie> {
        let date = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        // Walk only the domain buckets that can match this host. This keeps
        // request lookup proportional to the hostname depth instead of the
        // total number of domains in the process-wide jar.
        let domains = self.domains.read().unwrap_or_else(|e| e.into_inner());
        let mut matching: Vec<Cookie> = Vec::new();
        let mut accessed_domains: Vec<Arc<str>> = Vec::new();

        for domain_key in Self::domain_candidates(host) {
            let Some((stored_key, bucket)) = domains.get_key_value(domain_key.as_str()) else {
                continue;
            };
            let mut domain_accessed = false;
            for c in &bucket.cookies {
                if c.match_request(host, path, date, secure, is_cross_site) {
                    matching.push(c.clone());
                    domain_accessed = true;
                }
            }
            if domain_accessed {
                accessed_domains.push(Arc::clone(stored_key));
            }
        }
        drop(domains);

        // Update last_access_time for matched cookies and refresh domain LRU
        {
            let mut domains = self.domains.write().unwrap_or_else(|e| e.into_inner());
            for domain_key in &accessed_domains {
                if let Some(bucket) = domains.get_mut(domain_key) {
                    for c in &mut bucket.cookies {
                        if c.match_request(host, path, date, secure, is_cross_site) {
                            c.last_access_time = date;
                        }
                    }
                }
            }
        }
        for domain_key in &accessed_domains {
            self.update_lru(Arc::clone(domain_key));
        }

        // Sort per RFC 6265 Section 5.4: longer paths first, then earlier creation
        matching.sort_by(|a, b| {
            let depth_a = a.path.matches('/').count();
            let depth_b = b.path.matches('/').count();
            depth_b
                .cmp(&depth_a)
                .then_with(|| a.creation_time.cmp(&b.creation_time))
        });

        matching
    }

    pub fn find_cookies_for_url(&self, url: &reqwest::Url) -> Vec<Cookie> {
        self.find_cookies_for_url_with_context(url, false)
    }

    /// Find cookies for a URL with explicit cross-site context.
    pub fn find_cookies_for_url_with_context(
        &self,
        url: &reqwest::Url,
        is_cross_site: bool,
    ) -> Vec<Cookie> {
        let host = url.host_str().unwrap_or("");
        let path = url.path();
        let scheme = url.scheme();
        let secure = scheme == "https";
        self.find_cookies(host, path, secure, is_cross_site)
    }

    /// Remove all expired persistent cookies and return the count removed.
    pub fn expire_cookies(&self, base_time: i64) -> usize {
        let mut domains = self.domains.write().unwrap_or_else(|e| e.into_inner());
        let mut removed_total = 0;
        let mut empty_domains: Vec<Arc<str>> = Vec::new();

        for (domain_key, bucket) in domains.iter_mut() {
            let before = bucket.cookies.len();
            bucket.cookies.retain(|c| !c.is_expired(base_time));
            removed_total += before - bucket.cookies.len();
            if bucket.cookies.is_empty() {
                empty_domains.push(Arc::clone(domain_key));
            }
        }

        // Clean up empty domain buckets and their LRU entries
        for domain_key in &empty_domains {
            self.remove_from_lru(domain_key);
        }
        for domain_key in &empty_domains {
            domains.remove(domain_key);
        }

        removed_total
    }

    /// Returns the total number of cookies across all domains.
    pub fn count(&self) -> usize {
        let domains = self.domains.read().unwrap_or_else(|e| e.into_inner());
        domains.values().map(|b| b.cookies.len()).sum()
    }

    /// Returns the number of tracked domains (for testing).
    pub fn domain_count(&self) -> usize {
        self.lru_tracker
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Returns `true` if this storage contains a cookie with the same
    /// name, domain, and path as the given cookie.
    ///
    /// Equivalent to C++ `CookieStorage::contains()`.
    pub fn contains(&self, cookie: &Cookie) -> bool {
        let domains = self.domains.read().unwrap_or_else(|e| e.into_inner());
        if let Some(bucket) = domains.get(cookie.domain.as_str()) {
            return bucket.cookies.iter().any(|c| c == cookie);
        }
        false
    }

    /// Load cookies from a Netscape-format file or a browser SQLite cookie jar.
    ///
    /// Equivalent to C++ `CookieStorage::load()`. The format is detected from
    /// the file's first 16 bytes rather than its name — Firefox
    /// (`cookies.sqlite`) and Chromium (`Cookies`, no extension) both start
    /// with the SQLite magic, everything else is treated as Netscape
    /// `cookies.txt`. Each parsed cookie is stored via `add()`, which handles
    /// deduplication and eviction.
    pub fn load_file(&self, path: &Path) -> Result<usize> {
        let bytes = fs::read(path).map_err(|e| Aria2Error::Io(e.to_string()))?;

        let parsed = if is_sqlite_file(&bytes) {
            // The magic identifies SQLite but not the browser, so the schema is
            // probed (Firefox first, then Chromium), matching C++.
            Sqlite3CookieParser::parse_auto(path)?
        } else {
            // Netscape files are text; lossy conversion keeps a stray invalid
            // byte from discarding the whole jar.
            let text = String::from_utf8_lossy(&bytes);
            NsCookieParser::parse_str(&text)?
        };

        let n = parsed.len();
        for cookie in parsed {
            self.add(cookie);
        }
        Ok(n)
    }

    /// Save cookies to a file in Netscape format.
    ///
    /// Per C++ `CookieStorage::saveNsFormat()`: writes to a temp file first,
    /// then renames to the target path. This ensures atomicity — if the
    /// write fails or the process crashes, the original file is preserved.
    ///
    /// Cookies are iterated through the LRU tracker (matching C++ behavior
    /// which iterates `lruTracker_` to dump cookies).
    pub fn save_file(&self, path: &Path) -> Result<()> {
        let temp_path = {
            let mut p = path.to_path_buf();
            p.set_extension("tmp");
            p
        };

        {
            let domains = self.domains.read().unwrap_or_else(|e| e.into_inner());
            let lru = self.lru_tracker.read().unwrap_or_else(|e| e.into_inner());

            let mut lines = Vec::with_capacity(self.count() + 3);
            lines.push("# Netscape HTTP Cookie File".to_string());
            lines.push("# This file is generated by aria2-rust".to_string());

            // Iterate LRU tracker order (matches C++ saveNsFormat iteration)
            for (_, domain_key) in lru.iter() {
                if let Some(bucket) = domains.get(domain_key) {
                    for c in &bucket.cookies {
                        lines.push(c.to_netscape_line());
                    }
                }
            }

            fs::write(&temp_path, lines.join("\n")).map_err(|e| Aria2Error::Io(e.to_string()))?;
        }

        // Atomic rename (on Windows, this replaces the target if it exists)
        fs::rename(&temp_path, path).map_err(|e| Aria2Error::Io(e.to_string()))?;
        Ok(())
    }

    /// Remove all cookies and domain entries.
    pub fn clear(&self) {
        self.domains
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.lru_tracker
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    /// Format matching cookies as a Cookie header value string.
    pub fn to_header_string(&self, host: &str, path: &str, secure: bool) -> String {
        self.to_header_string_with_context(host, path, secure, false)
    }

    /// Format matching cookies as a Cookie header value string with SameSite context.
    pub fn to_header_string_with_context(
        &self,
        host: &str,
        path: &str,
        secure: bool,
        is_cross_site: bool,
    ) -> String {
        let cookies = self.find_cookies(host, path, secure, is_cross_site);
        if cookies.is_empty() {
            return String::new();
        }
        cookies
            .iter()
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn is_empty(&self) -> bool {
        self.domains
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Parse a Set-Cookie header and store the resulting cookie.
    ///
    /// Equivalent to C++ `CookieStorage::parseAndStore()`. Parses the
    /// Set-Cookie header string, validates the domain against the request
    /// host per RFC 6265, and stores the cookie if valid.
    ///
    /// Returns `true` if the cookie was stored or updated, `false` if the
    /// cookie was rejected (invalid format, domain mismatch) or deleted
    /// (Max-Age <= 0 for an existing cookie).
    pub fn parse_and_store(
        &self,
        set_cookie_string: &str,
        request_host: &str,
        request_path: &str,
    ) -> bool {
        let default_path = Cookie::default_path(request_path);
        let cookie =
            match Cookie::from_set_cookie_header(set_cookie_string, request_host, &default_path) {
                Some(c) => c,
                None => return false,
            };
        let mut cookie = cookie;
        cookie.domain = Self::canonical_domain(&cookie.domain);
        // Check if this is a delete cookie with no existing match
        if cookie.is_delete_cookie() {
            let mut domains = self.domains.write().unwrap_or_else(|e| e.into_inner());
            if let Some(bucket) = domains.get_mut(cookie.domain.as_str())
                && let Some(pos) = bucket.cookies.iter().position(|c| c == &cookie)
            {
                bucket.cookies.remove(pos);
                if bucket.cookies.is_empty() {
                    let domain = cookie.domain.clone();
                    drop(domains);
                    self.remove_from_lru(&domain);
                    self.domains
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(domain.as_str());
                }
                return false; // C++ returns false for expired cookies
            }
            return false;
        }
        self.add(cookie);
        true
    }

    /// Force eviction of `delnum` domains. Exposed for testing.
    ///
    /// Equivalent to C++ `CookieStorage::evictNode()`.
    pub fn evict_domains_for_test(&self, delnum: usize) {
        self.evict_domains(delnum);
    }
}

impl Default for CookieStorage {
    fn default() -> Self {
        CookieStorage::new()
    }
}
