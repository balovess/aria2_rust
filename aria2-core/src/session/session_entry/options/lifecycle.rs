use super::DownloadOptions;
use std::collections::HashMap;

pub(super) fn apply(opts: &DownloadOptions, map: &mut HashMap<String, String>) {
    // --- Task lifecycle / HTTP response policy ---
    if opts.pause {
        map.insert("pause".to_string(), "true".to_string());
    }
    if opts.pause_metadata {
        map.insert("pause-metadata".to_string(), "true".to_string());
    }
    if opts.force_save {
        map.insert("force-save".to_string(), "true".to_string());
    }
    map.insert(
        "save-not-found".to_string(),
        opts.save_not_found.to_string(),
    );
    map.insert(
        "rpc-save-upload-metadata".to_string(),
        opts.rpc_save_upload_metadata.to_string(),
    );
    if opts.content_disposition_default_utf8 {
        map.insert(
            "content-disposition-default-utf8".to_string(),
            "true".to_string(),
        );
    }
    if !opts.proxy_method.is_empty() {
        map.insert("proxy-method".to_string(), opts.proxy_method.clone());
    }
    map.insert(
        "max-file-not-found".to_string(),
        opts.max_file_not_found.to_string(),
    );

    // --- Authentication and netrc ---
    if opts.http_auth_challenge {
        map.insert("http-auth-challenge".to_string(), "true".to_string());
    }
    if let Some(ref v) = opts.http_user {
        map.insert("http-user".to_string(), v.clone());
    }
    if let Some(ref v) = opts.http_passwd {
        map.insert("http-passwd".to_string(), v.clone());
    }
    if let Some(ref v) = opts.ftp_user {
        map.insert("ftp-user".to_string(), v.clone());
    }
    if let Some(ref v) = opts.ftp_passwd {
        map.insert("ftp-passwd".to_string(), v.clone());
    }
    if opts.no_netrc {
        map.insert("no-netrc".to_string(), "true".to_string());
    }
    if let Some(ref v) = opts.netrc_path {
        map.insert("netrc-path".to_string(), v.clone());
    }
    if opts.conditional_get {
        map.insert("conditional-get".to_string(), "true".to_string());
    }

    // --- Metadata follow modes ---
    if let Some(mode) = opts.follow_torrent {
        map.insert("follow-torrent".to_string(), mode.as_str().to_string());
    }
    if let Some(mode) = opts.follow_metalink {
        map.insert("follow-metalink".to_string(), mode.as_str().to_string());
    }

    // --- Event hooks ---
    if let Some(ref v) = opts.on_download_start {
        map.insert("on-download-start".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_download_complete {
        map.insert("on-download-complete".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_download_error {
        map.insert("on-download-error".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_download_pause {
        map.insert("on-download-pause".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_download_stop {
        map.insert("on-download-stop".to_string(), v.clone());
    }
    if let Some(ref v) = opts.on_bt_download_complete {
        map.insert("on-bt-download-complete".to_string(), v.clone());
    }
}
