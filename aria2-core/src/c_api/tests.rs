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

#[derive(Default)]
struct DownloadEventCapture {
    events: Mutex<Vec<(usize, u32, u64, usize)>>,
}

unsafe extern "C" fn capture_download_event(
    session: *mut Aria2RustSession,
    event: u32,
    gid: u64,
    user_data: *mut c_void,
) -> i32 {
    if user_data.is_null() {
        return -1;
    }
    let capture = unsafe { &*user_data.cast::<DownloadEventCapture>() };
    capture
        .events
        .lock()
        .unwrap()
        .push((session as usize, event, gid, user_data as usize));
    0
}

fn serve_http_downloads(
    listener: std::net::TcpListener,
    expected_paths: [&'static str; 2],
) -> std::thread::JoinHandle<Vec<String>> {
    use std::io::{Read, Write};
    use std::time::Instant;

    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(12);
        let mut requests = Vec::new();
        let mut completed_paths = [false; 2];
        while !completed_paths.iter().all(|completed| *completed) && Instant::now() < deadline {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return requests;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("loopback HTTP accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0, "peer closed before sending HTTP headers");
                request.extend_from_slice(&buffer[..count]);
            }
            let body = b"c-api-event-callback";
            let request = String::from_utf8_lossy(&request);
            let mut lines = request.lines();
            let request_line = lines.next().unwrap_or_default().to_string();
            let mut request_parts = request_line.split_whitespace();
            let method = request_parts.next().unwrap_or_default().to_string();
            let path = request_parts.next().unwrap_or_default().to_string();
            requests.push(request_line);

            let response_body = if method == "GET" {
                for (index, expected_path) in expected_paths.iter().enumerate() {
                    if path == *expected_path {
                        completed_paths[index] = true;
                    }
                }
                body.as_slice()
            } else {
                &[]
            };
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                if method == "HEAD" {
                    body.len()
                } else {
                    response_body.len()
                }
            )
            .unwrap();
            stream.write_all(response_body).unwrap();
            stream.flush().unwrap();
        }
        requests
    })
}

fn serve_stalled_http_downloads(
    listener: std::net::TcpListener,
) -> (
    std::sync::mpsc::Receiver<usize>,
    std::thread::JoinHandle<usize>,
) {
    use std::io::{Read, Write};
    use std::time::Instant;

    listener.set_nonblocking(true).unwrap();
    let (get_tx, get_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut get_count = 0;
        while get_count < 2 && Instant::now() < deadline {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return get_count;
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("loopback HTTP accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let count = match stream.read(&mut buffer) {
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        return get_count;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::TimedOut => return get_count,
                    Err(error) => panic!("loopback HTTP request read failed: {error}"),
                };
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..count]);
            }
            let request_line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_string();
            let method = request_line.split_whitespace().next().unwrap_or_default();
            let body_length = 1024 * 1024;
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {body_length}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();
            if method == "GET" {
                get_count += 1;
                let _ = get_tx.send(get_count);
                loop {
                    match stream.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            return get_count;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                            return get_count;
                        }
                        Err(error) => panic!("loopback HTTP body read failed: {error}"),
                    }
                }
            }
        }
        get_count
    });
    (get_rx, server)
}

#[test]
fn c_api_event_callback_reports_pause_and_stop_for_active_http_download() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);

    let output_dir = tempfile::tempdir().unwrap();
    let (dir_entry, dir_name, dir_value) = kv("dir", output_dir.path().to_str().unwrap());
    let (tries_entry, tries_name, tries_value) = kv("max-tries", "1");
    let (keep_running_entry, keep_running_name, keep_running_value) = kv("keep-running", "true");
    let entries = [dir_entry, tries_entry, keep_running_entry];
    let _keep_alive = [
        dir_name,
        dir_value,
        tries_name,
        tries_value,
        keep_running_name,
        keep_running_value,
    ];

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (get_requests, server) = serve_stalled_http_downloads(listener);
    let mut capture = Box::<DownloadEventCapture>::default();
    let user_data = (&mut *capture as *mut DownloadEventCapture).cast::<c_void>();
    let session = unsafe {
        aria2_rust_session_new_with_download_event_callback(
            entries.as_ptr(),
            entries.len(),
            Some(capture_download_event),
            user_data,
        )
    };
    assert!(!session.is_null());

    let uri = CString::new(format!("http://{address}/stalled.bin")).unwrap();
    let uris = [uri.as_ptr()];
    let mut gid = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session, uris.as_ptr(), uris.len(), ptr::null(), 0, &mut gid) },
        0
    );

    let wait_for_get = |expected: usize| {
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            assert_eq!(unsafe { aria2_rust_run(session, 1) }, 1);
            if get_requests.recv_timeout(Duration::from_millis(2)).ok() == Some(expected) {
                return;
            }
        }
        let mut info = Aria2RustDownloadInfo::default();
        let info_result = unsafe { aria2_rust_get_download_info(session, gid, &mut info) };
        let stopped = unsafe {
            (&*session)
                .request_man
                .find_stopped_result(&format!("{gid:016x}"))
        };
        let events = capture.events.lock().unwrap().clone();
        panic!(
            "loopback server did not receive GET request #{expected}; info_result={info_result}, info={info:?}, stopped={stopped:?}, events={events:?}"
        );
    };
    wait_for_get(1);

    assert_eq!(unsafe { aria2_rust_pause(session, gid, 1) }, 0);
    let pause_deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        assert_eq!(unsafe { aria2_rust_run(session, 1) }, 1);
        let mut info = Aria2RustDownloadInfo::default();
        assert_eq!(
            unsafe { aria2_rust_get_download_info(session, gid, &mut info) },
            0
        );
        let saw_pause = capture
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, event, event_gid, _)| *event == 2 && *event_gid == gid);
        if info.status == Aria2RustDownloadStatus::Paused as u32 && saw_pause {
            break;
        }
        assert!(
            std::time::Instant::now() < pause_deadline,
            "download never reached callback-confirmed Paused state; status={}, events={:?}",
            info.status,
            capture.events.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    assert_eq!(unsafe { aria2_rust_unpause(session, gid) }, 0);
    wait_for_get(2);
    assert_eq!(unsafe { aria2_rust_remove(session, gid, 1) }, 0);
    let stop_deadline = std::time::Instant::now() + Duration::from_secs(8);
    loop {
        let run_result = unsafe { aria2_rust_run(session, 1) };
        let mut info = Aria2RustDownloadInfo::default();
        let info_result = unsafe { aria2_rust_get_download_info(session, gid, &mut info) };
        let saw_stop = capture
            .events
            .lock()
            .unwrap()
            .iter()
            .any(|(_, event, event_gid, _)| *event == 3 && *event_gid == gid);
        if info_result == 0 && info.status == Aria2RustDownloadStatus::Removed as u32 && saw_stop {
            break;
        }
        assert_eq!(run_result, 1);
        assert!(
            std::time::Instant::now() < stop_deadline,
            "download never reached callback-confirmed Removed state; info_result={info_result}, status={}, events={:?}",
            info.status,
            capture.events.lock().unwrap()
        );
        std::thread::sleep(Duration::from_millis(2));
    }

    let events = capture.events.lock().unwrap().clone();
    assert!(events.contains(&(session as usize, 1, gid, user_data as usize)));
    assert!(events.contains(&(session as usize, 2, gid, user_data as usize)));
    assert!(events.contains(&(session as usize, 3, gid, user_data as usize)));
    assert_eq!(unsafe { aria2_rust_session_final(session) }, 0);
    assert_eq!(server.join().unwrap(), 2);
    assert_eq!(aria2_rust_library_deinit(), 0);
}

#[test]
fn c_api_event_callback_is_session_scoped_for_real_http_downloads() {
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    assert_eq!(aria2_rust_library_init(), 0);

    let output_dir = tempfile::tempdir().unwrap();
    let (dir_entry, dir_name, dir_value) = kv("dir", output_dir.path().to_str().unwrap());
    let (tries_entry, tries_name, tries_value) = kv("max-tries", "1");
    let entries = [dir_entry, tries_entry];
    let _keep_alive = [dir_name, dir_value, tries_name, tries_value];

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = serve_http_downloads(listener, ["/a.bin", "/b.bin"]);

    let mut capture_a = Box::<DownloadEventCapture>::default();
    let mut capture_b = Box::<DownloadEventCapture>::default();
    let user_data_a = (&mut *capture_a as *mut DownloadEventCapture).cast::<c_void>();
    let user_data_b = (&mut *capture_b as *mut DownloadEventCapture).cast::<c_void>();
    let session_a = unsafe {
        aria2_rust_session_new_with_download_event_callback(
            entries.as_ptr(),
            entries.len(),
            Some(capture_download_event),
            user_data_a,
        )
    };
    let session_b = unsafe {
        aria2_rust_session_new_with_download_event_callback(
            entries.as_ptr(),
            entries.len(),
            Some(capture_download_event),
            user_data_b,
        )
    };
    assert!(!session_a.is_null());
    assert!(!session_b.is_null());

    let uri_a = CString::new(format!("http://{address}/a.bin")).unwrap();
    let uri_b = CString::new(format!("http://{address}/b.bin")).unwrap();
    let uris_a = [uri_a.as_ptr()];
    let uris_b = [uri_b.as_ptr()];
    let mut gid_a = 0;
    let mut gid_b = 0;
    assert_eq!(
        unsafe { aria2_rust_add_uri(session_a, uris_a.as_ptr(), 1, ptr::null(), 0, &mut gid_a) },
        0
    );
    assert_eq!(
        unsafe { aria2_rust_add_uri(session_b, uris_b.as_ptr(), 1, ptr::null(), 0, &mut gid_b) },
        0
    );
    assert_eq!(gid_a, gid_b, "test requires colliding per-session GIDs");

    let run_a = unsafe { aria2_rust_run(session_a, 0) };
    let run_b = unsafe { aria2_rust_run(session_b, 0) };
    let result_a = unsafe {
        (&*session_a)
            .request_man
            .find_stopped_result(&format!("{gid_a:016x}"))
    };
    let result_b = unsafe {
        (&*session_b)
            .request_man
            .find_stopped_result(&format!("{gid_b:016x}"))
    };
    let message_a = result_a.as_ref().map(|result| result.message.clone());
    let message_b = result_b.as_ref().map(|result| result.message.clone());
    let final_a = unsafe { aria2_rust_session_final(session_a) };
    let final_b = unsafe { aria2_rust_session_final(session_b) };
    let server_requests = server.join();
    let deinit = aria2_rust_library_deinit();

    assert_eq!(run_a, 0);
    assert_eq!(run_b, 0);
    assert_eq!(final_a, 0);
    assert_eq!(final_b, 0);
    assert_eq!(deinit, 0);
    let server_requests = server_requests.unwrap();
    for path in ["/a.bin", "/b.bin"] {
        assert!(
            server_requests
                .iter()
                .any(|request| request.starts_with(&format!("GET {path} "))),
            "loopback server did not receive GET for {path}: {server_requests:?}"
        );
    }

    // aria2_original's public DownloadEvent values: Start=1, Complete=4.
    for (session, gid, user_data, capture) in [
        (session_a as usize, gid_a, user_data_a as usize, &capture_a),
        (session_b as usize, gid_b, user_data_b as usize, &capture_b),
    ] {
        let events = capture.events.lock().unwrap();
        assert_eq!(
            events.len(),
            2,
            "callbacks must not cross session scope: {events:?}"
        );
        assert!(events.contains(&(session, 1, gid, user_data)));
        assert!(
            events.contains(&(session, 4, gid, user_data)),
            "expected completion callback; got {events:?}; task messages: {message_a:?}, {message_b:?}; HTTP requests: {server_requests:?}"
        );
    }
}
