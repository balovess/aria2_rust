use super::*;
use std::ffi::CString;
use std::ptr;

#[test]
fn c_api_sftp_timeout_global_options_reach_new_downloads() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let session = unsafe { aria2_rust_session_new(ptr::null(), 0, ptr::null_mut()) };
    assert!(!session.is_null());

    let options = [
        kv("sftp-connect-timeout", "41"),
        kv("sftp-read-timeout", "73"),
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
        0
    );

    let uri = CString::new("sftp://127.0.0.1:22/timeout-option-test").unwrap();
    let uri_ptr = uri.as_ptr();
    let mut gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &uri_ptr, 1, ptr::null(), 0, &mut gid) },
        0
    );
    let group = unsafe { (&*session).request_man.find_group(GroupId::new(gid)) }
        .expect("C API should create the SFTP request group");
    let group = group.read().unwrap();
    let options = group.options();
    assert_eq!(options.sftp_connect_timeout, Some(41));
    assert_eq!(options.sftp_read_timeout, Some(73));
    drop(group);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}
