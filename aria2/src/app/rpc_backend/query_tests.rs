use super::*;
use std::path::PathBuf;

#[cfg(feature = "bittorrent")]
#[test]
fn pending_magnet_status_and_get_files_use_the_resolved_output_path() {
    let command =
        aria2_core::engine::bittorrent::magnet::download_command::MagnetDownloadCommand::new(
            aria2_core::request::request_group::GroupId::new(42),
            "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&dn=example.bin",
            &aria2_core::request::request_group::DownloadOptions::default(),
            Some("downloads"),
        )
        .expect("valid magnet link");
    let group = command.group();
    let expected_path = PathBuf::from("downloads").join("example.bin");
    let expected_path = expected_path.to_string_lossy().into_owned();

    let status = CoreRpcBackend::status_from_group(&group, &group.gid().to_hex_string());
    let status_json = serde_json::to_value(status).expect("status serializes");
    assert_eq!(status_json["files"][0]["path"], expected_path);
    assert_eq!(
        build_file_infos(&group, 0)[0].path,
        expected_path,
        "aria2.getFiles shares the active-task file projection"
    );
}
