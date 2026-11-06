use super::*;

#[test]
fn default_directory_is_valid_for_a_safe_magnet_name() {
    let command = MagnetDownloadCommand::new(
        GroupId::new(4),
        "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&dn=test_file",
        &DownloadOptions::default(),
        Some("."),
    )
    .expect("a directory is not an output file");

    assert_eq!(
        command.output_path,
        std::path::PathBuf::from(".").join("test_file")
    );
}

#[test]
fn directory_output_name_is_rejected_before_io() {
    let options = DownloadOptions {
        out: Some(".".to_string()),
        ..DownloadOptions::default()
    };

    let error = match MagnetDownloadCommand::new(GroupId::new(5), TEST_MAGNET_URI, &options, None) {
        Ok(_) => panic!("a directory cannot be a magnet output file name"),
        Err(error) => error,
    };

    assert!(
        error
            .to_string()
            .contains("output name must be a file name")
    );
}

#[test]
fn unsafe_display_name_falls_back_to_a_file_name() {
    let command = MagnetDownloadCommand::new(
        GroupId::new(6),
        "magnet:?xt=urn:btih:abc123def45678901234567890abcdef12345678&dn=.",
        &DownloadOptions::default(),
        Some("."),
    )
    .expect("unsafe display names should use the fallback");

    assert_eq!(
        command.output_path,
        std::path::PathBuf::from(".").join("magnet_download")
    );
}
