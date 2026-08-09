//! Tests for domain-level LRU eviction in CookieStorage.
//!
//! Validates that the DOMAIN_EVICTION_TRIGGER and DOMAIN_EVICTION_RATE
//! constants produce the same behavior as C++ aria2's `evictNode()`.

use super::Cookie;
use super::storage::{CookieStorage, DOMAIN_EVICTION_RATE, DOMAIN_EVICTION_TRIGGER};

#[test]
fn test_domain_eviction_trigger_constant() {
    // Matches C++ aria2 DOMAIN_EVICTION_TRIGGER = 2000
    assert_eq!(DOMAIN_EVICTION_TRIGGER, 2000);
}

#[test]
fn test_domain_eviction_rate_constant() {
    // Matches C++ aria2 DOMAIN_EVICTION_RATE = 0.1
    assert_eq!(DOMAIN_EVICTION_RATE, 0.1);
}

#[test]
fn test_domain_count_tracks_unique_domains() {
    let store = CookieStorage::new();
    store.add(Cookie::new("a", "1", "alpha.com"));
    store.add(Cookie::new("b", "2", "alpha.com"));
    store.add(Cookie::new("c", "3", "beta.com"));

    // 2 unique domains tracked in LRU
    assert_eq!(store.domain_count(), 2);
}

#[test]
fn test_evict_domains_removes_domains() {
    let store = CookieStorage::new();

    // Add cookies to 3 domains
    store.add(Cookie::new("a", "1", "first.com"));
    store.add(Cookie::new("b", "2", "second.com"));
    store.add(Cookie::new("c", "3", "third.com"));

    assert_eq!(store.domain_count(), 3);
    assert_eq!(store.count(), 3);

    // Evict 1 domain
    store.evict_domains_for_test(1);
    assert_eq!(store.domain_count(), 2);
    assert_eq!(store.count(), 2);
}

#[test]
fn test_evict_domains_multiple() {
    let store = CookieStorage::new();

    // Add 5 domains
    for i in 0..5 {
        let domain = format!("d{}.com", i);
        store.add(Cookie::new("k", "v", &domain));
    }

    assert_eq!(store.domain_count(), 5);

    // Evict 3
    store.evict_domains_for_test(3);
    assert_eq!(store.domain_count(), 2);
    assert_eq!(store.count(), 2);
}

#[test]
fn test_evict_domains_clears_all_cookies_in_domain() {
    let store = CookieStorage::new();

    // Add multiple cookies to one domain
    store.add(Cookie::new("a", "1", "rich.com"));
    store.add(Cookie::new("b", "2", "rich.com"));
    store.add(Cookie::new("c", "3", "rich.com"));
    store.add(Cookie::new("x", "1", "other.com"));

    assert_eq!(store.count(), 4);

    // Evict 1 domain
    store.evict_domains_for_test(1);
    // At least one domain was evicted, total cookies decreased
    assert!(store.count() < 4);
}

#[test]
fn test_domain_lru_updated_on_add() {
    let store = CookieStorage::new();

    // Add 3 domains
    store.add(Cookie::new("a", "1", "d1.com"));
    store.add(Cookie::new("b", "2", "d2.com"));
    store.add(Cookie::new("c", "3", "d3.com"));

    // Access d1.com by adding another cookie — refreshes its LRU timestamp
    store.add(Cookie::new("d", "v", "d1.com"));

    // Evict 1 domain — should not be d1.com (its LRU was refreshed)
    store.evict_domains_for_test(1);
    assert_eq!(store.domain_count(), 2);

    // d1.com should survive because its LRU was just refreshed
    let found = store.find_cookies("d1.com", "/", false, false);
    assert!(
        !found.is_empty(),
        "d1.com should survive (LRU refreshed by add)"
    );
}

#[test]
fn test_domain_lru_updated_on_find() {
    let store = CookieStorage::new();

    // Add 3 domains
    store.add(Cookie::new("a", "1", "d1.com"));
    store.add(Cookie::new("b", "2", "d2.com"));
    store.add(Cookie::new("c", "3", "d3.com"));

    // Access d1.com via find_cookies — refreshes its LRU timestamp
    let found = store.find_cookies("d1.com", "/", false, false);
    assert_eq!(found.len(), 1, "d1.com cookie should be found");

    // Evict 1 domain — d1.com should not be evicted since its LRU was refreshed
    store.evict_domains_for_test(1);
    assert_eq!(store.domain_count(), 2);

    // d1.com should survive because find_cookies refreshed its LRU
    let found_after = store.find_cookies("d1.com", "/", false, false);
    assert!(
        !found_after.is_empty(),
        "d1.com should survive (LRU refreshed by find_cookies)"
    );
}

#[test]
fn test_per_domain_max_cookie_eviction() {
    let store = CookieStorage::new();

    // Add MAX_COOKIE_PER_DOMAIN cookies to one domain
    for i in 0..Cookie::max_cookie_per_domain() {
        let name = format!("k{}", i);
        let mut c = Cookie::new(&name, "v", "full.com");
        // Vary last_access_time so LRU eviction is deterministic
        c.last_access_time -= (Cookie::max_cookie_per_domain() - i) as i64;
        store.add(c);
    }

    assert_eq!(store.count(), Cookie::max_cookie_per_domain());
    assert_eq!(store.domain_count(), 1);

    // Adding one more should trigger per-domain LRU eviction
    let mut extra = Cookie::new("extra", "v", "full.com");
    extra.last_access_time += 1000; // Most recently accessed
    store.add(extra);

    // Should still be at max, not max+1
    assert_eq!(store.count(), Cookie::max_cookie_per_domain());
}

#[test]
fn test_delete_cookie_removes_empty_domain() {
    let store = CookieStorage::new();

    // Add a single cookie to a domain
    let mut c = Cookie::new("k", "v", "temp.com");
    c.persistent = true;
    c.expiry_time = i64::MAX; // Far future
    store.add(c.clone());
    assert_eq!(store.domain_count(), 1);

    // Delete the cookie by adding an expired version
    let mut del = Cookie::new("k", "v", "temp.com");
    del.persistent = true;
    del.expiry_time = 0; // Past expiry → delete cookie
    store.add(del);

    // Domain should be cleaned up since it's now empty
    assert_eq!(store.count(), 0);
    assert_eq!(store.domain_count(), 0);
}

#[test]
fn test_parse_and_store_delete_removes_empty_domain() {
    let store = CookieStorage::new();

    // Store a cookie
    let result = store.parse_and_store("k=v; path=/", "temp.com", "/");
    assert!(result);
    assert_eq!(store.domain_count(), 1);

    // Delete it with Max-Age=0
    let result = store.parse_and_store("k=deleted; Max-Age=0", "temp.com", "/");
    assert!(!result);
    assert_eq!(store.count(), 0);
    assert_eq!(store.domain_count(), 0);
}

#[test]
fn test_clear_resets_lru_tracker() {
    let store = CookieStorage::new();

    // Add several domains
    for i in 0..10 {
        let domain = format!("d{}.com", i);
        store.add(Cookie::new("k", "v", &domain));
    }
    assert_eq!(store.domain_count(), 10);

    // Clear everything
    store.clear();
    assert_eq!(store.domain_count(), 0);
    assert_eq!(store.count(), 0);
    assert!(store.is_empty());
}

#[test]
fn test_expire_cookies_removes_empty_domains() {
    let store = CookieStorage::new();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    // Add an about-to-expire cookie
    let mut c = Cookie::new("old", "v", "gone.com");
    c.persistent = true;
    c.expiry_time = now + 1;
    store.add(c);

    // Add a persistent cookie in another domain
    let mut c2 = Cookie::new("fresh", "v", "stay.com");
    c2.persistent = true;
    c2.expiry_time = i64::MAX;
    store.add(c2);

    assert_eq!(store.domain_count(), 2);

    // Expire with a future time
    let removed = store.expire_cookies(now + 10);
    assert_eq!(removed, 1);
    assert_eq!(store.domain_count(), 1);
}

#[test]
fn test_eviction_at_trigger_boundary() {
    // Test that eviction does NOT trigger below DOMAIN_EVICTION_TRIGGER.
    // The real trigger test would need 2000+ domains which is too slow
    // for unit tests; this just verifies no premature eviction occurs.
    let store = CookieStorage::new();

    // Add 10 domains — well below trigger
    for i in 0..10 {
        let domain = format!("domain{}.com", i);
        store.add(Cookie::new("k", "v", &domain));
    }

    assert_eq!(store.domain_count(), 10);
    assert_eq!(store.count(), 10);

    // No eviction should have occurred
    let found = store.find_cookies("domain5.com", "/", false, false);
    assert_eq!(found.len(), 1);
}

#[test]
fn test_evict_all_domains() {
    let store = CookieStorage::new();

    // Add 3 domains
    store.add(Cookie::new("a", "1", "x.com"));
    store.add(Cookie::new("b", "2", "y.com"));
    store.add(Cookie::new("c", "3", "z.com"));
    assert_eq!(store.domain_count(), 3);

    // Evict all 3
    store.evict_domains_for_test(3);
    assert_eq!(store.domain_count(), 0);
    assert_eq!(store.count(), 0);
    assert!(store.is_empty());
}

#[test]
fn test_evict_more_than_available() {
    let store = CookieStorage::new();

    store.add(Cookie::new("a", "1", "x.com"));
    store.add(Cookie::new("b", "2", "y.com"));
    assert_eq!(store.domain_count(), 2);

    // Evict 5 — should only remove 2 (all available)
    store.evict_domains_for_test(5);
    assert_eq!(store.domain_count(), 0);
    assert!(store.is_empty());
}

#[test]
fn test_eviction_rate_calculation() {
    // Verify the eviction rate calculation matches C++ behavior:
    // delnum = size_t(lruTracker_.size() * DOMAIN_EVICTION_RATE)
    // At trigger (2000 domains), delnum = size_t(2000 * 0.1) = 200
    let at_trigger = (DOMAIN_EVICTION_TRIGGER as f64 * DOMAIN_EVICTION_RATE) as usize;
    assert_eq!(at_trigger, 200, "At trigger, should evict 200 domains");
}

#[test]
fn test_find_cookies_refreshes_domain_lru() {
    let store = CookieStorage::new();

    // Add 3 domains
    store.add(Cookie::new("a", "1", "first.com"));
    store.add(Cookie::new("b", "2", "second.com"));
    store.add(Cookie::new("c", "3", "third.com"));

    // Access first.com via find_cookies — refreshes its LRU
    let found = store.find_cookies("first.com", "/", false, false);
    assert_eq!(found.len(), 1);

    // Evict 1 domain — the oldest LRU domain goes.
    // first.com was just refreshed, so it should not be the one evicted.
    store.evict_domains_for_test(1);
    assert_eq!(store.domain_count(), 2);

    // first.com survives because find_cookies refreshed its LRU
    let found_after = store.find_cookies("first.com", "/", false, false);
    assert!(
        !found_after.is_empty(),
        "first.com should survive (LRU refreshed by find_cookies)"
    );
}
