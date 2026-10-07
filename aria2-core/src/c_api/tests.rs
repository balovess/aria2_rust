use super::*;
use std::ffi::CString;
use std::sync::Mutex;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn kv(name: &str, value: &str) -> (Aria2RustKeyValue, CString, CString) {
    let name_c = CString::new(name).unwrap();
    let value_c = CString::new(value).unwrap();
    (
        Aria2RustKeyValue {
            name: name_c.as_ptr(),
            value: value_c.as_ptr(),
        },
        name_c,
        value_c,
    )
}

#[test]
fn c_api_session_lifecycle_and_queue_controls() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let (pause_entry, pause_name, pause_value) = kv("pause", "true");
    let session = unsafe { aria2_rust_session_new(&pause_entry, 1, ptr::null_mut()) };
    let _keep_alive = [pause_name, pause_value];
    assert!(!session.is_null());

    let uri = CString::new("http://127.0.0.1:1/not-started").unwrap();
    let uri_ptr = uri.as_ptr();
    let mut gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &uri_ptr, 1, ptr::null(), 0, &mut gid,) },
        0
    );
    assert_ne!(gid, 0);

    let mut info = Aria2RustDownloadInfo::default();
    let mut initial_info_result = -1;
    for _ in 0..100 {
        initial_info_result = unsafe { aria2_rust_get_download_info(session, gid, &mut info) };
        if initial_info_result == 0 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(initial_info_result, 0);
    assert_eq!(info.status, Aria2RustDownloadStatus::Paused as u32);

    let mut unpause_result = -1;
    for _ in 0..100 {
        unpause_result = unsafe { aria2_rust_unpause(session, gid) };
        if unpause_result == 0 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(unpause_result, 0);
    // Re-pause immediately so the short-lived test URI cannot race the
    // engine into a terminal network error before the state assertion.
    let mut pause_result = -1;
    for _ in 0..100 {
        pause_result = unsafe { aria2_rust_pause(session, gid, 1) };
        if pause_result == 0 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(pause_result, 0);
    let mut info_result = -1;
    for _ in 0..100 {
        info_result = unsafe { aria2_rust_get_download_info(session, gid, &mut info) };
        if info_result == 0 && info.status == Aria2RustDownloadStatus::Paused as u32 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(info_result, 0);
    assert_eq!(info.status, Aria2RustDownloadStatus::Paused as u32);

    let mut remove_result = -1;
    for _ in 0..100 {
        remove_result = unsafe { aria2_rust_remove(session, gid, 1) };
        if remove_result == 0 {
            break;
        }
        std::thread::yield_now();
    }
    assert_eq!(remove_result, 0);
    let mut removed_info_result = -1;
    for _ in 0..2000 {
        removed_info_result = unsafe { aria2_rust_get_download_info(session, gid, &mut info) };
        if removed_info_result == 0 && info.status == Aria2RustDownloadStatus::Removed as u32 {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(removed_info_result, 0);
    assert_eq!(info.status, Aria2RustDownloadStatus::Removed as u32);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}

#[test]
fn c_api_wait_download_is_event_driven_and_timeout_safe() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let (pause_entry, pause_name, pause_value) = kv("pause", "true");
    let session = unsafe { aria2_rust_session_new(&pause_entry, 1, ptr::null_mut()) };
    let _keep_alive = [pause_name, pause_value];
    assert!(!session.is_null());

    let uri = CString::new("http://127.0.0.1:1/not-started").unwrap();
    let uri_ptr = uri.as_ptr();
    let mut gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &uri_ptr, 1, ptr::null(), 0, &mut gid) },
        0
    );

    let mut info = Aria2RustDownloadInfo::default();
    assert_eq!(
        unsafe { aria2_rust_wait_download(session, gid, 1, &mut info) },
        TIMEOUT
    );

    unsafe {
        (&*session)
            .request_man
            .find_group(GroupId::new(gid))
            .expect("download group")
            .recover()
            .mark_complete();
    }
    assert_eq!(
        unsafe { aria2_rust_wait_download(session, gid, 1_000, &mut info) },
        0
    );
    assert_eq!(info.status, Aria2RustDownloadStatus::Complete as u32);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}

#[cfg(feature = "bittorrent")]
#[test]
fn c_api_torrent_file_metadata_is_available_before_start() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let (pause_entry, pause_name, pause_value) = kv("pause", "true");
    let session = unsafe { aria2_rust_session_new(&pause_entry, 1, ptr::null_mut()) };
    let _keep_alive = [pause_name, pause_value];
    assert!(!session.is_null());

    let torrent =
            b"d8:announce27:http://example.com/announce4:infod6:lengthi4e4:name8:file.bin12:piece lengthi4e6:pieces20:12345678901234567890eee";
    let mut gid = 0;
    assert_eq!(
        unsafe {
            aria2_rust_add_torrent(
                session,
                torrent.as_ptr(),
                torrent.len(),
                ptr::null(),
                0,
                ptr::null(),
                0,
                &mut gid,
            )
        },
        0
    );

    assert_eq!(unsafe { aria2_rust_get_file_count(session, gid) }, 1);
    let mut info = Aria2RustFileInfo::default();
    assert_eq!(
        unsafe { aria2_rust_get_file_info(session, gid, 1, &mut info) },
        0
    );
    assert_eq!(info.length, 4);
    assert_eq!(info.completed_length, 0);
    assert_eq!(info.selected, 1);

    let required = unsafe { aria2_rust_get_file_path(session, gid, 1, ptr::null_mut(), 0) };
    assert!(required > 1);
    let mut path = vec![0 as c_char; required];
    assert_eq!(
        unsafe { aria2_rust_get_file_path(session, gid, 1, path.as_mut_ptr(), path.len()) },
        required
    );
    let path = unsafe { CStr::from_ptr(path.as_ptr()) }.to_str().unwrap();
    assert!(path.ends_with("file.bin"));

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}

#[cfg(feature = "metalink")]
#[test]
fn c_api_metalink_returns_all_created_gids() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let (pause_entry, pause_name, pause_value) = kv("pause", "true");
    let session = unsafe { aria2_rust_session_new(&pause_entry, 1, ptr::null_mut()) };
    let _keep_alive = [pause_name, pause_value];
    assert!(!session.is_null());

    let metalink = br#"<metalink xmlns="urn:ietf:params:xml:ns:metalink"><file name="file.bin"><size>4</size><url>http://127.0.0.1:1/file.bin</url></file></metalink>"#;
    let mut gids = [0u64; 2];
    let mut gid_count = 0usize;
    assert_eq!(
        unsafe {
            aria2_rust_add_metalink(
                session,
                metalink.as_ptr(),
                metalink.len(),
                ptr::null(),
                0,
                gids.as_mut_ptr(),
                gids.len(),
                &mut gid_count,
            )
        },
        0
    );
    assert_eq!(gid_count, 1);
    assert_ne!(gids[0], 0);
    assert_eq!(unsafe { aria2_rust_get_file_count(session, gids[0]) }, 1);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}

#[test]
fn c_api_batch_controls_and_queue_position_match_library_semantics() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let (pause_entry, pause_name, pause_value) = kv("pause", "true");
    let session = unsafe { aria2_rust_session_new(&pause_entry, 1, ptr::null_mut()) };
    let _keep_alive = [pause_name, pause_value];
    assert!(!session.is_null());

    let first = CString::new("http://127.0.0.1:1/first").unwrap();
    let second = CString::new("http://127.0.0.1:1/second").unwrap();
    let first_ptr = first.as_ptr();
    let second_ptr = second.as_ptr();
    let mut first_gid = 0;
    let mut second_gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &first_ptr, 1, ptr::null(), 0, &mut first_gid) },
        0
    );
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &second_ptr, 1, ptr::null(), 0, &mut second_gid) },
        0
    );

    assert_eq!(unsafe { aria2_rust_pause_all(session, 1) }, 0);
    let mut info = Aria2RustDownloadInfo::default();
    assert_eq!(
        unsafe { aria2_rust_get_download_info(session, first_gid, &mut info) },
        0
    );
    assert_eq!(info.status, Aria2RustDownloadStatus::Paused as u32);

    let mut new_position = usize::MAX;
    assert_eq!(
        unsafe {
            aria2_rust_change_position(
                session,
                second_gid,
                0,
                ARIA2_RUST_POSITION_SET,
                &mut new_position,
            )
        },
        0
    );
    assert_eq!(new_position, 0);
    assert_eq!(unsafe { aria2_rust_unpause_all(session) }, 0);
    assert_eq!(
        unsafe { aria2_rust_get_download_info(session, first_gid, &mut info) },
        0
    );
    assert_eq!(info.status, Aria2RustDownloadStatus::Waiting as u32);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}

#[test]
fn c_api_uses_shared_option_conversion_and_ignores_unknown_options() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let (entry, name, value) = kv("split", "4");
    let (unknown, unknown_name, unknown_value) = kv("not-an-aria2-option", "ignored");
    let entries = [entry, unknown];
    let _keep_alive = [name, value, unknown_name, unknown_value];
    let session =
        unsafe { aria2_rust_session_new(entries.as_ptr(), entries.len(), ptr::null_mut()) };
    assert!(!session.is_null());
    let uri = CString::new("http://127.0.0.1:1/option-test").unwrap();
    let uri_ptr = uri.as_ptr();
    let mut gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &uri_ptr, 1, ptr::null(), 0, &mut gid,) },
        0
    );
    assert_ne!(gid, 0);
    let mut hex = [0 as c_char; 17];
    assert_eq!(aria2_rust_gid_to_hex(gid, hex.as_mut_ptr(), hex.len()), 17);
    assert_eq!(unsafe { aria2_rust_hex_to_gid(hex.as_ptr()) }, gid);
    unsafe { aria2_rust_session_final(session) };
    aria2_rust_library_deinit();
}

#[test]
fn c_api_change_option_uses_reserved_group_semantics() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let (pause_entry, pause_name, pause_value) = kv("pause", "true");
    let session = unsafe { aria2_rust_session_new(&pause_entry, 1, ptr::null_mut()) };
    let _keep_alive = [pause_name, pause_value];
    assert!(!session.is_null());
    let (dir_entry, dir_name, dir_value) = kv("dir", "reserved-dir");
    let _keep_alive_option = [dir_name, dir_value];

    let uri = CString::new("http://127.0.0.1:1/c-api-change-option").unwrap();
    let uri_ptr = uri.as_ptr();
    let mut gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &uri_ptr, 1, ptr::null(), 0, &mut gid) },
        0
    );

    assert_eq!(
        unsafe { aria2_rust_change_option(session, gid, &dir_entry, 1,) },
        0
    );

    {
        let group = unsafe { (&*session).request_man.find_group(GroupId::new(gid)) }
            .expect("C API option change should keep the group");
        let group = group.read().unwrap();
        assert_eq!(group.options().dir.as_deref(), Some("reserved-dir"));
        assert_eq!(
            group.runtime_options().get("dir"),
            Some(&serde_json::json!("reserved-dir"))
        );
        assert!(group.pending_options().is_empty());
    }

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}
