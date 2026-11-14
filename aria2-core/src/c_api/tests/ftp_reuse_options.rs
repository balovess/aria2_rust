use super::*;
use std::ptr;

#[test]
fn c_api_ftp_reuse_global_and_task_options_reach_download_groups() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let session = unsafe { aria2_rust_session_new(ptr::null(), 0, ptr::null_mut()) };
    assert!(!session.is_null());

    let global_value = kv("ftp-reuse-connection", "false");
    let global_entry = global_value.0;
    assert_eq!(
        unsafe { aria2_rust_change_global_option(session, &global_entry, 1) },
        0
    );

    let global_uri = CString::new("ftp://127.0.0.1:1/global.bin").unwrap();
    let global_uri_ptr = global_uri.as_ptr();
    let mut global_gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &global_uri_ptr, 1, ptr::null(), 0, &mut global_gid) },
        0
    );
    let global_group = unsafe { (&*session).request_man.find_group(GroupId::new(global_gid)) }
        .expect("C API should apply the global FTP reuse setting");
    assert!(!global_group.read().unwrap().options().ftp_reuse_connection);

    let task_uri = CString::new("ftp://127.0.0.1:1/task.bin").unwrap();
    let task_uri_ptr = task_uri.as_ptr();
    let task_value = kv("ftp-reuse-connection", "true");
    let task_entry = task_value.0;
    let mut task_gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, &task_uri_ptr, 1, &task_entry, 1, &mut task_gid,) },
        0
    );
    let task_group = unsafe { (&*session).request_man.find_group(GroupId::new(task_gid)) }
        .expect("C API should apply the per-task FTP reuse setting");
    assert!(task_group.read().unwrap().options().ftp_reuse_connection);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}
