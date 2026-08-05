//! Metadata/payload request graph construction for Metalink torrent metaurls.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use crate::error::{Aria2Error, Result};
use crate::request::request_group::{
    BtDependency, DownloadOptions, GroupId, MetadataInfo, RequestGroup,
};
use crate::util::rwlock_ext::RwLockRecover;

/// The two groups required to resolve a Metalink torrent metaurl.
///
/// The metadata group downloads the `.torrent` file. The payload group stays
/// reserved until [`BtDependency`] injects the parsed torrent context.
pub struct MetalinkRequestGraph {
    pub metadata: Arc<RwLock<RequestGroup>>,
    pub payload: Arc<RwLock<RequestGroup>>,
    pub metadata_path: PathBuf,
}

impl MetalinkRequestGraph {
    /// Construct a metadata group and a dependency-gated payload group.
    pub fn new(
        metadata_uri: &str,
        payload_name: &str,
        options: &DownloadOptions,
        metadata_gid: GroupId,
        payload_gid: GroupId,
    ) -> Result<Self> {
        if metadata_uri.is_empty() || payload_name.is_empty() {
            return Err(Aria2Error::Fatal(crate::error::FatalError::Config(
                "Metalink torrent graph requires metadata URI and payload name".to_string(),
            )));
        }

        let output_dir = options.dir.as_deref().unwrap_or(".");
        let metadata_name = metadata_filename(metadata_uri, metadata_gid);
        let metadata_name = if metadata_name == payload_name {
            format!("{}.torrent", metadata_gid.to_hex_string())
        } else {
            metadata_name
        };
        let metadata_path = Path::new(output_dir).join(&metadata_name);
        let payload_path = Path::new(output_dir).join(payload_name);

        let mut metadata_options = options.clone();
        metadata_options.out = Some(metadata_name);
        let metadata = Arc::new(RwLock::new(RequestGroup::new(
            metadata_gid,
            vec![metadata_uri.to_string()],
            metadata_options,
        )));
        let payload = Arc::new(RwLock::new(RequestGroup::new(
            payload_gid,
            vec![format!("bt://{}", metadata_gid.to_hex_string())],
            options.clone(),
        )));

        payload.recover().set_following_gid(metadata_gid);
        payload.recover().set_belongs_to_gid(metadata_gid);
        metadata.recover().add_followed_by_gid(payload_gid);

        let metadata_info = MetadataInfo::new(metadata_gid, metadata_uri)
            .with_metadata_path(metadata_path.to_string_lossy());
        payload.recover().set_metadata_info(metadata_info.clone());
        payload
            .recover()
            .set_dependency(Box::new(BtDependency::new_file(
                metadata_gid,
                Arc::clone(&payload),
                metadata_path.clone(),
                payload_path,
                metadata_info,
            )));

        Ok(Self {
            metadata,
            payload,
            metadata_path,
        })
    }
}

fn metadata_filename(uri: &str, gid: GroupId) -> String {
    url::Url::parse(uri)
        .ok()
        .and_then(|url| {
            Path::new(url.path())
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| format!("{}.torrent", gid.to_hex_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_completion_resolves_and_promotes_payload() {
        let manager = crate::request::request_group_man::RequestGroupMan::new();
        let graph = MetalinkRequestGraph::new(
            "https://example.test/releases/payload.torrent",
            "payload.bin",
            &DownloadOptions::default(),
            GroupId::new(20),
            GroupId::new(21),
        )
        .expect("graph should be constructible");
        let metadata_path = graph.metadata_path.clone();
        std::fs::write(&metadata_path, minimal_torrent()).expect("metadata should be writable");
        manager.add_metalink_graph(graph).unwrap();

        let first = manager.fill_from_reserver();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].recover().gid(), GroupId::new(20));

        manager.resolve_dependencies_for(GroupId::new(20));
        let second = manager.fill_from_reserver();
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].recover().gid(), GroupId::new(21));
        assert!(second[0].recover().get_download_context().is_some());

        let _ = std::fs::remove_file(metadata_path);
    }

    fn minimal_torrent() -> Vec<u8> {
        let mut data = b"d8:announce28:http://tracker.test/announce4:infod6:lengthi0e4:name4:test12:piece lengthi16384e6:pieces20:".to_vec();
        data.extend_from_slice(&[
            0xda, 0x39, 0xa3, 0xee, 0x5e, 0x6b, 0x4b, 0x0d, 0x32, 0x55, 0xbf, 0xef, 0x95, 0x60,
            0x18, 0x90, 0xaf, 0xd8, 0x07, 0x09,
        ]);
        data.extend_from_slice(b"ee");
        data
    }

    #[test]
    fn creates_dependency_gated_metadata_payload_graph() {
        let graph = MetalinkRequestGraph::new(
            "https://example.test/releases/payload.torrent",
            "payload.bin",
            &DownloadOptions::default(),
            GroupId::new(10),
            GroupId::new(11),
        )
        .expect("graph should be constructible");

        assert_eq!(graph.metadata.recover().gid(), GroupId::new(10));
        assert_eq!(graph.payload.recover().gid(), GroupId::new(11));
        assert_eq!(
            graph.payload.recover().following_gid(),
            Some(GroupId::new(10))
        );
        assert_eq!(
            graph.payload.recover().belongs_to_gid(),
            Some(GroupId::new(10))
        );
        assert!(!graph.payload.recover().is_dependency_resolved());
        assert_eq!(
            graph
                .metadata_path
                .file_name()
                .and_then(|name| name.to_str()),
            Some("payload.torrent")
        );
    }
}
