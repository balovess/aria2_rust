use super::*;
use std::ptr;

#[test]
fn c_api_http_auth_global_and_task_options_reach_download_groups() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let session = unsafe { aria2_rust_session_new(ptr::null(), 0, ptr::null_mut()) };
    assert!(!session.is_null());

    let global_values = [
        kv("http-user", "global-user"),
        kv("http-passwd", "global-pass"),
        kv("http-auth-challenge", "true"),
        kv("no-netrc", "true"),
        kv("header", "Authorization: Bearer global"),
    ];
    let global_entries = global_values
        .iter()
        .map(|(entry, _, _)| Aria2RustKeyValue {
            name: entry.name,
            value: entry.value,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        unsafe {
            aria2_rust_change_global_option(session, global_entries.as_ptr(), global_entries.len())
        },
        0
    );

    let global_uri = CString::new("http://127.0.0.1:1/http-auth-global").unwrap();
    let global_uri_ptr = global_uri.as_ptr();
    let mut global_gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &global_uri_ptr, 1, ptr::null(), 0, &mut global_gid) },
        0
    );
    let global_group = unsafe { (&*session).request_man.find_group(GroupId::new(global_gid)) }
        .expect("C API should apply global HTTP auth options to the task");
    {
        let group = global_group.read().unwrap();
        let options = group.options();
        assert_eq!(options.http_user.as_deref(), Some("global-user"));
        assert_eq!(options.http_passwd.as_deref(), Some("global-pass"));
        assert!(options.http_auth_challenge);
        assert!(options.no_netrc);
        assert_eq!(options.header, ["Authorization: Bearer global"]);
    }

    let task_uri = CString::new("http://127.0.0.1:1/http-auth-task").unwrap();
    let task_uri_ptr = task_uri.as_ptr();
    let task_values = [
        kv("http-user", "task-user"),
        kv("http-passwd", "task-pass"),
        kv("header", "Authorization: Bearer task"),
    ];
    let task_entries = task_values
        .iter()
        .map(|(entry, _, _)| Aria2RustKeyValue {
            name: entry.name,
            value: entry.value,
        })
        .collect::<Vec<_>>();
    let mut task_gid = 0;
    assert_eq!(
        unsafe {
            aria2_rust_add_uri(
                session,
                &task_uri_ptr,
                1,
                task_entries.as_ptr(),
                task_entries.len(),
                &mut task_gid,
            )
        },
        0
    );
    let task_group = unsafe { (&*session).request_man.find_group(GroupId::new(task_gid)) }
        .expect("C API should apply per-task HTTP auth options");
    {
        let group = task_group.read().unwrap();
        let options = group.options();
        assert_eq!(options.http_user.as_deref(), Some("task-user"));
        assert_eq!(options.http_passwd.as_deref(), Some("task-pass"));
        assert!(options.http_auth_challenge);
        assert!(options.no_netrc);
        assert_eq!(options.header, ["Authorization: Bearer task"]);
    }

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}
