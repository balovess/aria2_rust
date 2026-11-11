use super::*;

#[tokio::test]
#[ignore = "requires public Debian WebSeed availability"]
async fn public_debian_web_seed_follows_redirect_for_piece_range() {
    let url = "https://cdimage.debian.org/cdimage/release/13.7.0/amd64/iso-dvd/debian-13.7.0-amd64-DVD-1.iso";
    let total_length = 3_992_977_408;
    let piece_length = 262_144;
    let entry = crate::download::file_entry::FileEntry::new(
        "debian-13.7.0-amd64-DVD-1.iso".into(),
        total_length,
        0,
        vec![url.to_string()],
    );
    let mut context = crate::download::DownloadContext::new_default();
    context.set_piece_length(piece_length);
    context.set_file_entries(vec![entry]);
    let group = std::sync::Arc::new(std::sync::RwLock::new(
        crate::request::request_group::RequestGroup::new(
            crate::request::request_group::GroupId::new(7102),
            Vec::new(),
            Default::default(),
        ),
    ));
    group
        .recover()
        .set_download_context(std::sync::Arc::new(context));
    let manager = WebSeedManager::for_request_group(
        group,
        piece_length,
        total_length,
        crate::http::client_identity::ClientTlsConfig::default(),
        crate::network::OutboundNetworkPolicy::direct().into(),
    );

    let data = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        manager.request_piece_with_length_and_activity(42, piece_length as u64, None),
    )
    .await
    .expect("public WebSeed range request should not hang")
    .expect("public Debian WebSeed should serve a redirecting range request");

    assert_eq!(data.len(), piece_length as usize);
}
