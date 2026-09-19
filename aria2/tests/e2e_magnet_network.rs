//! Opt-in end-to-end coverage for a real public magnet link.
//!
//! This test intentionally stays ignored in normal CI: the exact source,
//! public trackers, and Internet routing are outside the repository's control.
//! Run it explicitly with:
//! `cargo test -p aria2 --test e2e_magnet_network -- --ignored --nocapture`

use std::process::Command;

const BIG_BUCK_BUNNY_MAGNET: &str = "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c&dn=Big+Buck+Bunny&tr=udp%3A%2F%2Fexplodie.org%3A6969&tr=udp%3A%2F%2Ftracker.coppersurfer.tk%3A6969&tr=udp%3A%2F%2Ftracker.empire-js.us%3A1337&tr=udp%3A%2F%2Ftracker.leechers-paradise.org%3A6969&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337&tr=wss%3A%2F%2Ftracker.btorrent.xyz&tr=wss%3A%2F%2Ftracker.fastcast.nz&tr=wss%3A%2F%2Ftracker.openwebtorrent.com&ws=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2F&xs=https%3A%2F%2Fwebtorrent.io%2Ftorrents%2Fbig-buck-bunny.torrent";

#[test]
#[ignore = "requires access to the public exact source or trackers"]
fn real_magnet_cli_resolves_and_saves_metadata() {
    let output_dir = tempfile::tempdir().expect("temporary CLI output directory");
    let output = Command::new(env!("CARGO_BIN_EXE_aria2c"))
        .env("HTTP_PROXY", "")
        .env("HTTPS_PROXY", "")
        .env("ALL_PROXY", "")
        .env("http_proxy", "")
        .env("https_proxy", "")
        .env("all_proxy", "")
        .env("NO_PROXY", "")
        .env("no_proxy", "")
        .args([
            "--no-conf",
            "--no-color",
            "--summary-interval=0",
            "--bt-metadata-only=true",
            "--bt-save-metadata=true",
            "--enable-dht=false",
            "--max-tries=1",
            "--timeout=15",
            "--bt-tracker-timeout=5",
            "--bt-tracker-connect-timeout=5",
            "--dir",
            output_dir.path().to_str().unwrap(),
        ])
        .arg(BIG_BUCK_BUNNY_MAGNET)
        .output()
        .expect("aria2c process must start");

    assert!(
        output.status.success(),
        "real magnet CLI failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let metadata_path = output_dir
        .path()
        .join("dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c.torrent");
    let metadata = std::fs::read(&metadata_path)
        .unwrap_or_else(|error| panic!("saved metadata missing at {:?}: {error}", metadata_path));
    let torrent = aria2_protocol::bittorrent::torrent::parser::TorrentMeta::parse(&metadata)
        .expect("saved metadata must be a valid torrent");
    assert_eq!(
        torrent.info_hash.as_hex(),
        "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c"
    );
}
