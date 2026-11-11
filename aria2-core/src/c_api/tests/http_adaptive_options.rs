use super::*;
use std::ffi::CString;
use std::ptr;

#[test]
fn c_api_http_adaptive_global_options_reach_new_downloads() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let session = unsafe { aria2_rust_session_new(ptr::null(), 0, ptr::null_mut()) };
    assert!(!session.is_null());

    let option_names = [
        "connect-timeout",
        "http-adaptive-enabled",
        "http-adaptive-reduction-percent",
        "http-adaptive-recovery-interval",
        "http-adaptive-recovery-successes-min",
        "http-adaptive-recovery-successes-max",
        "http-adaptive-start-interval-ms",
        "http-adaptive-backoff-base",
        "http-adaptive-backoff-max",
        "http-adaptive-capacity-start-interval-ms",
        "http-adaptive-capacity-start-interval-max-ms",
        "http-adaptive-rate-recovery-successes",
        "http-adaptive-rate-min-interval-ms",
        "http-adaptive-backoff-reset-after",
    ];
    let options = option_names
        .iter()
        .map(|name| {
            kv(
                name,
                if *name == "http-adaptive-enabled" {
                    "false"
                } else {
                    "1"
                },
            )
        })
        .collect::<Vec<_>>();
    let entries = options
        .iter()
        .map(|(_, name, value)| Aria2RustKeyValue {
            name: name.as_ptr(),
            value: value.as_ptr(),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        unsafe { aria2_rust_change_global_option(session, entries.as_ptr(), entries.len()) },
        0
    );

    let uri = CString::new("http://127.0.0.1:1/http-adaptive-option-test").unwrap();
    let uri_ptr = uri.as_ptr();
    let mut gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &uri_ptr, 1, ptr::null(), 0, &mut gid) },
        0
    );
    let group = unsafe { (&*session).request_man.find_group(GroupId::new(gid)) }
        .expect("C API should create a request group using the global options");
    let group = group.read().unwrap();
    let adaptive = group.options().http_adaptive;
    assert_eq!(group.options().connect_timeout, Some(1));
    assert!(!adaptive.enabled);
    assert_eq!(adaptive.reduction_percent, 1);
    assert_eq!(adaptive.recovery_interval_secs, 1);
    assert_eq!(adaptive.recovery_successes_min, 1);
    assert_eq!(adaptive.recovery_successes_max, 1);
    assert_eq!(adaptive.start_interval_ms, 1);
    assert_eq!(adaptive.backoff_base_secs, 1);
    assert_eq!(adaptive.backoff_max_secs, 1);
    assert_eq!(adaptive.capacity_start_interval_ms, 1);
    assert_eq!(adaptive.capacity_start_interval_max_ms, 1);
    assert_eq!(adaptive.rate_recovery_successes, 1);
    assert_eq!(adaptive.rate_min_interval_ms, 1);
    assert_eq!(adaptive.backoff_reset_after_secs, 1);
    drop(group);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    aria2_rust_library_deinit();
}

#[test]
fn c_api_rejects_a_global_option_batch_without_partial_mutation() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let session = unsafe { aria2_rust_session_new(ptr::null(), 0, ptr::null_mut()) };
    assert!(!session.is_null());

    let before_dir = unsafe {
        (&*session)
            .runtime
            .block_on((&*session).config.get_global_str("dir"))
            .expect("global dir has a default")
    };
    let options = [
        kv("keep-running", "true"),
        kv("dir", "should-not-be-applied"),
        kv("file-allocation", "invalid-value"),
    ];
    let entries = options
        .iter()
        .map(|(entry, _, _)| Aria2RustKeyValue {
            name: entry.name,
            value: entry.value,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        unsafe { aria2_rust_change_global_option(session, entries.as_ptr(), entries.len()) },
        INVALID_ARGUMENT
    );

    let session_ref = unsafe { &*session };
    assert!(!session_ref.keep_running);
    assert_eq!(
        session_ref
            .runtime
            .block_on(session_ref.config.get_global_str("dir")),
        Some(before_dir)
    );

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    aria2_rust_library_deinit();
}

#[test]
fn c_api_ignores_options_outside_the_global_change_policy() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let session = unsafe { aria2_rust_session_new(ptr::null(), 0, ptr::null_mut()) };
    assert!(!session.is_null());

    let before_interface = unsafe {
        (&*session)
            .runtime
            .block_on((&*session).config.get_global_str("interface"))
    };
    let (entry, _name, _value) = kv("interface", "127.0.0.2");
    assert_eq!(
        unsafe { aria2_rust_change_global_option(session, &entry, 1) },
        0
    );
    let after_interface = unsafe {
        (&*session)
            .runtime
            .block_on((&*session).config.get_global_str("interface"))
    };
    assert_eq!(after_interface, before_interface);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    aria2_rust_library_deinit();
}
