use crate::bittorrent::peer::connection::PeerAddr;

#[derive(Debug, Clone)]
pub enum PexMessage {
    Added(Vec<PeerAddr>),
    Removed(Vec<PeerAddr>),
}

pub struct PexHandler;

impl PexHandler {
    pub const EXTENSION_NAME: &'static str = "ut_pex";
    pub const EXTENSION_ID: u8 = 1;

    pub fn parse_pex_data(_data: &[u8]) -> Result<PexMessage, String> {
        Err("PEX解析暂未实现".to_string())
    }

    pub fn build_pex_message(added: Vec<PeerAddr>, removed: Vec<PeerAddr>) -> PexMessage {
        PexMessage::Added(added)
    }

    pub fn is_supported_by_peer(extension_ids: &[Option<u8>]) -> bool {
        extension_ids.iter().any(|id| *id == Some(Self::EXTENSION_ID))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pex_support_detection() {
        assert!(PexHandler::is_supported_by_peer(&[Some(1)]));
        assert!(!PexHandler::is_supported_by_peer(&[Some(2)]));
        assert!(!PexHandler::is_supported_by_peer(&[None]));
    }

    #[test]
    fn test_build_pex_message() {
        let addr = PeerAddr::new("1.2.3.4", 5678);
        let msg = PexHandler::build_pex_message(vec![addr.clone()], vec![]);
        match msg {
            PexMessage::Added(peers) => assert_eq!(peers[0].ip, "1.2.3.4"),
            _ => panic!("Expected Added message"),
        }
    }
}
