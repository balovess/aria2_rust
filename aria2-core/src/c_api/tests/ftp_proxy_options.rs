use super::*;
use std::ptr;

#[test]
fn c_api_ftp_proxy_global_options_reach_new_downloads() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let session = unsafe { aria2_rust_session_new(ptr::null(), 0, ptr::null_mut()) };
    assert!(!session.is_null());

    let options = [
        kv("ftp-proxy", "http://proxy.example:3128"),
        kv("ftp-proxy-user", "proxy-user"),
        kv("ftp-proxy-passwd", "proxy-password"),
        kv("proxy-method", "tunnel"),
        kv("user-agent", "task-agent/1.0"),
        kv("ftp-user", "download-user"),
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

    let uri = CString::new("ftp://example.test/file.bin").unwrap();
    let uri_ptr = uri.as_ptr();
    let mut gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &uri_ptr, 1, ptr::null(), 0, &mut gid) },
        0
    );
    let group = unsafe { (&*session).request_man.find_group(GroupId::new(gid)) }
        .expect("C API should create the FTP request group");
    let group = group.read().unwrap();
    let options = group.options();
    assert_eq!(
        options.ftp_proxy.as_deref(),
        Some("http://proxy.example:3128")
    );
    assert_eq!(options.ftp_proxy_user.as_deref(), Some("proxy-user"));
    assert_eq!(options.ftp_proxy_passwd.as_deref(), Some("proxy-password"));
    assert_eq!(options.proxy_method, "tunnel");
    assert_eq!(options.user_agent.as_deref(), Some("task-agent/1.0"));
    assert_eq!(options.ftp_user.as_deref(), Some("download-user"));
    drop(group);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}
