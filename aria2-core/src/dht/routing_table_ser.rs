//! DHT routing table serialization/deserialization (dht.dat).
//!
//! Implements the binary format used by C++ aria2 for persisting the DHT
//! routing table across restarts. The format is versioned with a magic header
//! and supports both IPv4 and IPv6 compact address representations.
//!
//! # File Format (Version 3)
//!
//! ```text
//! [Header: 8 bytes]
//!   bytes 0-1: magic 0xA1A2
//!   byte  2:   format ID 0x02
//!   bytes 3-5: reserved (zero)
//!   bytes 6-7: version 0x0003 (big-endian)
//!
//! [Timestamp: 8 bytes]
//!   uint64_t big-endian: seconds since Unix epoch
//!
//! [Local Node: 32 bytes]
//!   8 bytes:  reserved (zero)
//!   20 bytes: local node ID
//!   4 bytes:  reserved (zero)
//!
//! [Node Count: 8 bytes]
//!   4 bytes: uint32_t big-endian: number of nodes
//!   4 bytes: reserved (zero)
//!
//! [Per-Node Record: 56 bytes each]
//!   1 byte:          compact peer info length (6=IPv4, 18=IPv6)
//!   7 bytes:         reserved (zero)
//!   compactlen bytes: compact IP + port
//!   24-compactlen:   reserved (zero-padded)
//!   20 bytes:        node ID
//!   4 bytes:         reserved (zero)
//! ```
//!
//! C++ reference: `DHTRoutingTableSerializer.cc` / `DHTRoutingTableDeserializer.cc`

use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use super::constants::ID_LENGTH;
use super::node::DhtNode;
use super::node_id::NodeId;
use super::routing_table::RoutingTable;

// ── Constants ──────────────────────────────────────────────────────────────

/// File magic: first two bytes of a valid dht.dat file.
const MAGIC: [u8; 2] = [0xA1, 0xA2];

/// Format ID: byte 2 of the header.
const FORMAT_ID: u8 = 0x02;

/// Current serialization version (bytes 6-7, big-endian).
const VERSION: u16 = 3;

/// Minimum supported version (v2 is also readable for backward compat).
const MIN_VERSION: u16 = 2;

/// Compact peer info length for IPv4 (4 bytes IP + 2 bytes port).
const COMPACT_LEN_IPV4: usize = 6;

/// Compact peer info length for IPv6 (16 bytes IP + 2 bytes port).
const COMPACT_LEN_IPV6: usize = 18;

/// Size of each per-node record (fixed at 56 bytes regardless of address family).
#[allow(dead_code)]
const NODE_RECORD_SIZE: usize = 56;

// ── Serialize ──────────────────────────────────────────────────────────────

/// Serialize the routing table to a writer in C++ aria2 dht.dat format.
///
/// Writes a version 3 binary file compatible with the C++ implementation.
/// The writer is typically a `std::fs::File`. For atomic saves, write to a
/// temp file first and then rename.
///
/// # Errors
///
/// Returns `io::Error` on write failures.
pub fn serialize_to_writer(table: &RoutingTable, mut writer: impl Write) -> io::Result<()> {
    // ── Header (8 bytes) ────────────────────────────────────────────────
    let mut header = [0u8; 8];
    header[0] = MAGIC[0];
    header[1] = MAGIC[1];
    header[2] = FORMAT_ID;
    // bytes 3-5: reserved (zero)
    header[6..8].copy_from_slice(&VERSION.to_be_bytes());
    writer.write_all(&header)?;

    // ── Timestamp (8 bytes) ─────────────────────────────────────────────
    let epoch_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    writer.write_all(&epoch_secs.to_be_bytes())?;

    // ── Local node (32 bytes) ───────────────────────────────────────────
    let zero8 = [0u8; 8];
    let zero4 = [0u8; 4];
    writer.write_all(&zero8)?; // 8 bytes reserved
    writer.write_all(table.local_node_id().as_bytes())?; // 20 bytes node ID
    writer.write_all(&zero4)?; // 4 bytes reserved

    // ── Collect all nodes from routing table ─────────────────────────────
    let buckets = table.get_buckets();
    let all_nodes: Vec<&DhtNode> = buckets.iter().flat_map(|b| b.nodes().iter().map(|n| n.as_ref())).collect();

    // ── Node count (8 bytes) ────────────────────────────────────────────
    let num_nodes = all_nodes.len() as u32;
    writer.write_all(&num_nodes.to_be_bytes())?;
    writer.write_all(&zero4)?; // 4 bytes reserved

    // ── Per-node records (56 bytes each) ────────────────────────────────
    for node in &all_nodes {
        write_node_record(&mut writer, node)?;
    }

    debug!(
        nodes = all_nodes.len(),
        "DHT routing table serialized successfully"
    );
    Ok(())
}

/// Write a single node record (56 bytes) to the writer.
fn write_node_record(mut writer: impl Write, node: &DhtNode) -> io::Result<()> {
    let zero7 = [0u8; 7];
    let zero4 = [0u8; 4];

    let addr = node.addr();
    let (compact, compact_len) = pack_compact_peer(addr);

    // 1 byte: compact peer info length
    writer.write_all(&[compact_len as u8])?;
    // 7 bytes: reserved
    writer.write_all(&zero7)?;
    // compactlen bytes: compact IP + port
    writer.write_all(&compact[..compact_len])?;
    // 24 - compactlen bytes: reserved (zero-padded)
    let padding = 24 - compact_len;
    let zero_buf = [0u8; 24];
    writer.write_all(&zero_buf[..padding])?;
    // 20 bytes: node ID
    writer.write_all(node.id().as_bytes())?;
    // 4 bytes: reserved
    writer.write_all(&zero4)?;

    Ok(())
}

// ── Deserialize ────────────────────────────────────────────────────────────

/// Deserialization result containing the local node ID and discovered nodes.
#[derive(Debug)]
pub struct DeserializeResult {
    /// The local node ID read from the file.
    pub local_node_id: NodeId,
    /// Timestamp when the file was saved (seconds since Unix epoch).
    pub saved_at: u64,
    /// Discovered nodes from the routing table.
    pub nodes: Vec<DhtNode>,
}

/// Deserialize a dht.dat file from a reader.
///
/// Supports both version 2 and version 3 formats for backward compatibility
/// with C++ aria2 generated files.
///
/// # Errors
///
/// Returns `io::Error` on read failures or `io::ErrorKind::InvalidData` for
/// unrecognized file formats.
pub fn deserialize_from_reader(mut reader: impl Read) -> io::Result<DeserializeResult> {
    // ── Header (8 bytes) ────────────────────────────────────────────────
    let mut header = [0u8; 8];
    reader.read_exact(&mut header)?;

    // Validate magic
    if header[0] != MAGIC[0] || header[1] != MAGIC[1] {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DHT routing table: bad magic bytes",
        ));
    }

    // Validate format ID
    if header[2] != FORMAT_ID {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "DHT routing table: unsupported format ID",
        ));
    }

    // Extract version
    let version = u16::from_be_bytes([header[6], header[7]]);
    if version < MIN_VERSION || version > VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("DHT routing table: unsupported version {}", version),
        ));
    }

    // ── Timestamp ───────────────────────────────────────────────────────
    let saved_at = if version == 2 {
        // Version 2: 4-byte timestamp + 4 bytes reserved
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf)?;
        let ts = u32::from_be_bytes(buf);
        // 4 bytes reserved
        let mut skip = [0u8; 4];
        reader.read_exact(&mut skip)?;
        ts as u64
    } else {
        // Version 3: 8-byte timestamp
        let mut buf = [0u8; 8];
        reader.read_exact(&mut buf)?;
        u64::from_be_bytes(buf)
    };

    // ── Local node (32 bytes) ───────────────────────────────────────────
    let mut skip8 = [0u8; 8];
    reader.read_exact(&mut skip8)?; // 8 bytes reserved

    let mut id_buf = [0u8; ID_LENGTH];
    reader.read_exact(&mut id_buf)?; // 20 bytes node ID
    let local_node_id = NodeId::from_slice(&id_buf);

    let mut skip4 = [0u8; 4];
    reader.read_exact(&mut skip4)?; // 4 bytes reserved

    // ── Node count (8 bytes) ────────────────────────────────────────────
    let mut count_buf = [0u8; 4];
    reader.read_exact(&mut count_buf)?;
    let num_nodes = u32::from_be_bytes(count_buf);
    reader.read_exact(&mut skip4)?; // 4 bytes reserved

    // ── Per-node records ────────────────────────────────────────────────
    let mut nodes = Vec::with_capacity(num_nodes as usize);
    for _ in 0..num_nodes {
        match read_node_record(&mut reader) {
            Ok(Some(node)) => nodes.push(node),
            Ok(None) => continue, // skip invalid entry
            Err(e) => {
                warn!("Error reading DHT node record: {}", e);
                break; // stop reading on I/O error
            }
        }
    }

    debug!(
        nodes = nodes.len(),
        saved_at,
        "DHT routing table deserialized successfully"
    );

    Ok(DeserializeResult {
        local_node_id,
        saved_at,
        nodes,
    })
}

/// Read a single node record (56 bytes) from the reader.
///
/// Returns `Ok(Some(node))` on success, `Ok(None)` if the entry should be
/// skipped (invalid address, zeroed compact info, etc.), or an I/O error.
fn read_node_record(mut reader: impl Read) -> io::Result<Option<DhtNode>> {
    let mut skip7 = [0u8; 7];
    let mut skip4 = [0u8; 4];

    // 1 byte: compact peer info length
    let mut clen_buf = [0u8; 1];
    reader.read_exact(&mut clen_buf)?;
    let compact_len = clen_buf[0] as usize;

    // 7 bytes: reserved
    reader.read_exact(&mut skip7)?;

    // Validate compact length: must be 6 (IPv4) or 18 (IPv6)
    if compact_len != COMPACT_LEN_IPV4 && compact_len != COMPACT_LEN_IPV6 {
        // Skip the rest of this record (7 + 24 + 20 + 4 = 55 remaining,
        // but we already read 8 bytes, so 48 more)
        let mut skip = [0u8; 48];
        reader.read_exact(&mut skip)?;
        return Ok(None);
    }

    // compactlen bytes: compact IP + port
    let mut compact = [0u8; COMPACT_LEN_IPV6]; // max size
    reader.read_exact(&mut compact[..compact_len])?;

    // Check for all-zero compact info (indicates an invalid/empty entry)
    if compact[..compact_len].iter().all(|&b| b == 0) {
        // Skip remaining bytes: (24 - compactlen) + 20 + 4
        let remaining = (24 - compact_len) + ID_LENGTH + 4;
        let mut skip = [0u8; 48]; // max remaining is 48
        reader.read_exact(&mut skip[..remaining])?;
        return Ok(None);
    }

    // Unpack compact peer info to SocketAddr
    let addr = match unpack_compact_peer(&compact[..compact_len]) {
        Some(a) => a,
        None => {
            // Skip remaining bytes
            let remaining = (24 - compact_len) + ID_LENGTH + 4;
            let mut skip = [0u8; 48];
            reader.read_exact(&mut skip[..remaining])?;
            return Ok(None);
        }
    };

    // 24 - compactlen bytes: reserved
    let padding = 24 - compact_len;
    if padding > 0 {
        let mut skip = [0u8; 24];
        reader.read_exact(&mut skip[..padding])?;
    }

    // 20 bytes: node ID
    let mut id_buf = [0u8; ID_LENGTH];
    reader.read_exact(&mut id_buf)?;
    let node_id = NodeId::from_slice(&id_buf);

    // 4 bytes: reserved
    reader.read_exact(&mut skip4)?;

    Ok(Some(DhtNode::new(node_id, addr)))
}

// ── Compact peer helpers ───────────────────────────────────────────────────

/// Pack a `SocketAddr` into compact peer format.
///
/// Returns `(compact_bytes, length)`. The returned buffer is always 18 bytes
/// (max for IPv6), but only the first `length` bytes are meaningful.
fn pack_compact_peer(addr: SocketAddr) -> ([u8; COMPACT_LEN_IPV6], usize) {
    use std::net::IpAddr;
    let mut buf = [0u8; COMPACT_LEN_IPV6];
    match addr.ip() {
        IpAddr::V4(v4) => {
            buf[..4].copy_from_slice(&v4.octets());
            buf[4..6].copy_from_slice(&addr.port().to_be_bytes());
            (buf, COMPACT_LEN_IPV4)
        }
        IpAddr::V6(v6) => {
            buf[..16].copy_from_slice(&v6.octets());
            buf[16..18].copy_from_slice(&addr.port().to_be_bytes());
            (buf, COMPACT_LEN_IPV6)
        }
    }
}

/// Unpack compact peer format bytes into a `SocketAddr`.
///
/// Handles both IPv4 (6 bytes) and IPv6 (18 bytes) formats.
fn unpack_compact_peer(data: &[u8]) -> Option<SocketAddr> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    match data.len() {
        COMPACT_LEN_IPV4 => {
            let mut ip_bytes = [0u8; 4];
            ip_bytes.copy_from_slice(&data[..4]);
            let port = u16::from_be_bytes([data[4], data[5]]);
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip_bytes)), port))
        }
        COMPACT_LEN_IPV6 => {
            let mut ip_bytes = [0u8; 16];
            ip_bytes.copy_from_slice(&data[..16]);
            let port = u16::from_be_bytes([data[16], data[17]]);
            Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip_bytes)), port))
        }
        _ => None,
    }
}

// ── File-level convenience functions ────────────────────────────────────────

/// Serialize the routing table to a file (dht.dat).
///
/// Writes atomically by first saving to a temp file, then renaming.
/// This mirrors the C++ implementation's `serialize(filename)` method.
pub fn serialize_to_file(table: &RoutingTable, path: &std::path::Path) -> io::Result<()> {
    let temp_path = path.with_extension("dat__temp");
    {
        let file = std::fs::File::create(&temp_path)?;
        serialize_to_writer(table, file)?;
    }
    std::fs::rename(&temp_path, path)?;
    debug!("DHT routing table saved to {:?}", path);
    Ok(())
}

/// Deserialize a routing table from a file (dht.dat).
///
/// Returns the local node ID, timestamp, and discovered nodes.
/// Returns an error if the file doesn't exist or has an invalid format.
pub fn deserialize_from_file(path: &std::path::Path) -> io::Result<DeserializeResult> {
    let file = std::fs::File::open(path)?;
    deserialize_from_reader(file)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn test_addr_v4() -> SocketAddr {
        SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 6881)
    }

    fn test_addr_v6() -> SocketAddr {
        SocketAddr::new(IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)), 6881)
    }

    fn make_node(id_byte: u8, addr: SocketAddr) -> DhtNode {
        let id = NodeId::from_slice(&[id_byte; ID_LENGTH]);
        DhtNode::new(id, addr)
    }

    #[test]
    fn serialize_deserialize_roundtrip_ipv4() {
        let local_id = NodeId::from_slice(&[0x80; ID_LENGTH]);
        let mut table = RoutingTable::new(local_id);

        // Add some nodes
        for i in 1u8..5 {
            let mut node = make_node(i, SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(10, 0, 0, i)),
                6881 + i as u16,
            ));
            node.mark_good();
            node.update_last_contact();
            table.add_node(node);
        }

        // Serialize
        let mut buf = Vec::new();
        serialize_to_writer(&table, &mut buf).unwrap();

        // Deserialize
        let result = deserialize_from_reader(&buf[..]).unwrap();

        assert_eq!(result.local_node_id, local_id);
        assert!(result.saved_at > 0);
        assert_eq!(result.nodes.len(), 4);

        // Verify node IDs match
        for node in &result.nodes {
            assert_eq!(node.id().as_bytes()[0], node.addr().port() as u8 - 6881);
        }
    }

    #[test]
    fn serialize_deserialize_roundtrip_ipv6() {
        let local_id = NodeId::from_slice(&[0x80; ID_LENGTH]);
        let mut table = RoutingTable::new(local_id);

        let mut node = make_node(0x01, test_addr_v6());
        node.mark_good();
        node.update_last_contact();
        table.add_node(node);

        let mut buf = Vec::new();
        serialize_to_writer(&table, &mut buf).unwrap();

        let result = deserialize_from_reader(&buf[..]).unwrap();
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].addr(), test_addr_v6());
    }

    #[test]
    fn header_magic_validation() {
        let bad_data = [0u8; 64];
        let result = deserialize_from_reader(&bad_data[..]);
        assert!(result.is_err());
        assert!(result.unwrap_err().kind() == io::ErrorKind::InvalidData);
    }

    #[test]
    fn version_2_backward_compat() {
        // Construct a version 2 file manually
        let mut buf = Vec::new();

        // Header (version 2)
        let mut header = [0u8; 8];
        header[0] = MAGIC[0];
        header[1] = MAGIC[1];
        header[2] = FORMAT_ID;
        header[6] = 0;
        header[7] = 0x02; // version 2
        buf.extend_from_slice(&header);

        // Timestamp: 4 bytes + 4 reserved
        buf.extend_from_slice(&1000u32.to_be_bytes());
        buf.extend_from_slice(&[0u8; 4]);

        // Local node: 8 reserved + 20 ID + 4 reserved
        buf.extend_from_slice(&[0u8; 8]);
        buf.extend_from_slice(&[0xAB; ID_LENGTH]);
        buf.extend_from_slice(&[0u8; 4]);

        // Node count: 4 bytes + 4 reserved (0 nodes)
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&[0u8; 4]);

        let result = deserialize_from_reader(&buf[..]).unwrap();
        assert_eq!(result.saved_at, 1000);
        assert_eq!(result.local_node_id.as_bytes(), &[0xAB; ID_LENGTH]);
        assert!(result.nodes.is_empty());
    }

    #[test]
    fn compact_peer_pack_unpack_ipv4() {
        let (packed, len) = pack_compact_peer(test_addr_v4());
        assert_eq!(len, COMPACT_LEN_IPV4);
        let addr = unpack_compact_peer(&packed[..len]).unwrap();
        assert_eq!(addr, test_addr_v4());
    }

    #[test]
    fn compact_peer_pack_unpack_ipv6() {
        let (packed, len) = pack_compact_peer(test_addr_v6());
        assert_eq!(len, COMPACT_LEN_IPV6);
        let addr = unpack_compact_peer(&packed[..len]).unwrap();
        assert_eq!(addr, test_addr_v6());
    }

    #[test]
    fn empty_routing_table_serializes() {
        let local_id = NodeId::from_slice(&[0x80; ID_LENGTH]);
        let table = RoutingTable::new(local_id);

        let mut buf = Vec::new();
        serialize_to_writer(&table, &mut buf).unwrap();

        let result = deserialize_from_reader(&buf[..]).unwrap();
        assert_eq!(result.local_node_id, local_id);
        assert!(result.nodes.is_empty());
    }

    #[test]
    fn file_roundtrip() {
        let dir = std::env::temp_dir().join("aria2_rust_dht_test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dht.dat");

        let local_id = NodeId::from_slice(&[0x42; ID_LENGTH]);
        let mut table = RoutingTable::new(local_id);
        let mut node = make_node(0x01, test_addr_v4());
        node.mark_good();
        node.update_last_contact();
        table.add_node(node);

        serialize_to_file(&table, &path).unwrap();
        let result = deserialize_from_file(&path).unwrap();

        assert_eq!(result.local_node_id, local_id);
        assert_eq!(result.nodes.len(), 1);

        // Cleanup
        let _ = std::fs::remove_dir_all(&dir);
    }
}
