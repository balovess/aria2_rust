//! Extension registration logic: construction and peer handshake processing.

use std::collections::HashMap;

use aria2_protocol::bittorrent::message::extension::ExtensionHandshake;
use tracing::{debug, trace, warn};

use super::{
    DEFAULT_LOCAL_UT_METADATA_ID, DEFAULT_LOCAL_UT_PEX_ID, DEFAULT_REQQ, ExtensionRegistry,
    UT_METADATA_NAME, UT_PEX_NAME,
};

impl ExtensionRegistry {
    /// Create a new registry with default local ext_id assignments.
    ///
    /// Local assignments: ut_metadata = 1, ut_pex = 2.
    /// Peer assignments are empty until a handshake is received.
    pub fn new() -> Self {
        let mut local_extensions = HashMap::new();
        local_extensions.insert(UT_METADATA_NAME.to_vec(), DEFAULT_LOCAL_UT_METADATA_ID);
        local_extensions.insert(UT_PEX_NAME.to_vec(), DEFAULT_LOCAL_UT_PEX_ID);

        Self {
            local_extensions,
            peer_extensions: HashMap::new(),
            peer_id_to_name: HashMap::new(),
            reqq: DEFAULT_REQQ,
        }
    }

    /// Update peer extension assignments from a received extension handshake.
    ///
    /// Parses the `m` dict from the handshake and populates `peer_extensions`
    /// and the reverse map `peer_id_to_name`. Also stores the `reqq` value.
    ///
    /// This should be called exactly once, when the extension handshake
    /// (ext_id = 0) is received from the peer.
    pub fn update_from_peer_handshake(&mut self, handshake: &ExtensionHandshake) {
        // Clear previous peer mappings (shouldn't happen, but be safe)
        self.peer_extensions.clear();
        self.peer_id_to_name.clear();

        // Walk the m_dict and register each extension
        for (name, value) in handshake.m_dict() {
            if let Some(ext_id) = value.as_int().and_then(|i| u8::try_from(i).ok()) {
                if ext_id == 0 {
                    // ext_id 0 is reserved for the handshake itself
                    warn!(
                        "Peer assigned ext_id=0 to extension {:?}, ignoring",
                        String::from_utf8_lossy(name)
                    );
                    continue;
                }
                trace!(
                    "Peer extension: {:?} -> ext_id={}",
                    String::from_utf8_lossy(name),
                    ext_id
                );
                self.peer_extensions.insert(name.clone(), ext_id);
                self.peer_id_to_name.insert(ext_id, name.clone());
            }
        }

        self.reqq = handshake.reqq();

        debug!(
            "Updated extension registry from peer handshake: ut_metadata={:?}, ut_pex={:?}, reqq={}",
            self.peer_ut_metadata_id(),
            self.peer_ut_pex_id(),
            self.reqq
        );
    }

    /// Build our local extension handshake to send to the peer.
    ///
    /// Creates an `ExtensionHandshake` with our local ext_id assignments
    /// and the default reqq value.
    pub fn build_local_handshake(&self) -> ExtensionHandshake {
        let mut hs = ExtensionHandshake::new();
        hs.with_ut_metadata(self.local_ut_metadata_id());
        hs.with_ut_pex(self.local_ut_pex_id());
        hs.with_reqq(self.reqq);
        hs
    }
}
