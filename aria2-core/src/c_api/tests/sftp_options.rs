use super::*;
use std::ffi::CString;
use std::ptr;

#[test]
fn c_api_sftp_options_global_and_task_values_reach_new_downloads() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);
    let session = unsafe { aria2_rust_session_new(ptr::null(), 0, ptr::null_mut()) };
    assert!(!session.is_null());

    let options = [
        kv("ssh-host-key-md", "sha-1=global-fingerprint"),
        kv("sftp-connect-timeout", "41"),
        kv("sftp-read-timeout", "73"),
        kv("sftp-compression", "true"),
        kv("sftp-keepalive-interval", "0"),
        kv(
            "sftp-preferred-ciphers",
            "aes256-gcm@openssh.com,chacha20-poly1305@openssh.com",
        ),
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
    assert_eq!(
        options.ssh_host_key_md.as_deref(),
        Some("sha-1=global-fingerprint")
    );
    assert_eq!(options.sftp_connect_timeout, Some(41));
    assert_eq!(options.sftp_read_timeout, Some(73));
    assert!(options.sftp_compression);
    assert_eq!(options.sftp_keepalive_interval, Some(0));
    assert_eq!(
        options.sftp_preferred_ciphers.as_deref(),
        Some(
            &[
                "aes256-gcm@openssh.com".to_string(),
                "chacha20-poly1305@openssh.com".to_string()
            ][..]
        )
    );
    drop(group);

    let task_options = [
        kv("ssh-host-key-md", "md5=task-fingerprint"),
        kv("sftp-connect-timeout", "19"),
        kv("sftp-compression", "false"),
    ];
    let task_entries = task_options
        .iter()
        .map(|(entry, _, _)| Aria2RustKeyValue {
            name: entry.name,
            value: entry.value,
        })
        .collect::<Vec<_>>();
    let task_uri = CString::new("sftp://127.0.0.1:22/task-option-test").unwrap();
    let task_uri_ptr = task_uri.as_ptr();
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
        .expect("C API should apply the per-task SFTP options");
    let task_group = task_group.read().unwrap();
    let task_options = task_group.options();
    assert_eq!(
        task_options.ssh_host_key_md.as_deref(),
        Some("md5=task-fingerprint")
    );
    assert_eq!(task_options.sftp_connect_timeout, Some(19));
    assert_eq!(task_options.sftp_read_timeout, Some(73));
    assert!(!task_options.sftp_compression);
    assert_eq!(task_options.sftp_keepalive_interval, Some(0));
    drop(task_group);

    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(aria2_rust_library_deinit(), 0);
}
