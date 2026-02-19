use std::io::Read;

use incrementalmerkletree::frontier::CommitmentTree;
use orchard::tree::MerkleHashOrchard;

/// Compute the Orchard note commitment tree root from a lightwalletd frontier hex string.
///
/// The hex string is the `orchard_tree` field from a lightwalletd `TreeState` response.
/// It encodes a legacy `CommitmentTree` using the Zcash serialization format:
///   Optional<Node> left, Optional<Node> right, CompactSize + Vec<Optional<Node>> parents
///
/// Each node is 32 bytes (Pallas base field element, little-endian).
pub fn root_from_frontier_hex(hex_str: &str) -> anyhow::Result<[u8; 32]> {
    if hex_str.is_empty() {
        return Ok(CommitmentTree::<MerkleHashOrchard, { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 }>::empty()
            .root()
            .to_bytes());
    }

    let bytes = hex::decode(hex_str)?;
    let mut cursor = &bytes[..];

    let left = read_optional_hash(&mut cursor)?;
    let right = read_optional_hash(&mut cursor)?;
    let count = read_compact_size(&mut cursor)?;
    let parents: Vec<Option<MerkleHashOrchard>> = (0..count)
        .map(|_| read_optional_hash(&mut cursor))
        .collect::<Result<_, _>>()?;

    let tree = CommitmentTree::<MerkleHashOrchard, { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 }>::from_parts(
        left, right, parents,
    )
    .map_err(|_| anyhow::anyhow!("invalid commitment tree: parents exceed tree depth"))?;

    Ok(tree.root().to_bytes())
}

fn read_optional_hash(reader: &mut &[u8]) -> anyhow::Result<Option<MerkleHashOrchard>> {
    let mut flag = [0u8; 1];
    reader.read_exact(&mut flag)?;
    match flag[0] {
        0 => Ok(None),
        1 => {
            let mut buf = [0u8; 32];
            reader.read_exact(&mut buf)?;
            let hash = <Option<MerkleHashOrchard>>::from(MerkleHashOrchard::from_bytes(&buf))
                .ok_or_else(|| anyhow::anyhow!("non-canonical Pallas base field element in tree node"))?;
            Ok(Some(hash))
        }
        _ => Err(anyhow::anyhow!("invalid optional flag byte: {}", flag[0])),
    }
}

/// Read a Bitcoin-style CompactSize unsigned integer.
fn read_compact_size(reader: &mut &[u8]) -> anyhow::Result<usize> {
    let mut b = [0u8; 1];
    reader.read_exact(&mut b)?;
    match b[0] {
        n @ 0..=252 => Ok(n as usize),
        253 => {
            let mut buf = [0u8; 2];
            reader.read_exact(&mut buf)?;
            Ok(u16::from_le_bytes(buf) as usize)
        }
        254 => {
            let mut buf = [0u8; 4];
            reader.read_exact(&mut buf)?;
            Ok(u32::from_le_bytes(buf) as usize)
        }
        255 => {
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf)?;
            Ok(u64::from_le_bytes(buf) as usize)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_frontier_returns_empty_root() {
        let root = root_from_frontier_hex("").unwrap();
        let expected = CommitmentTree::<MerkleHashOrchard, { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 }>::empty()
            .root()
            .to_bytes();
        assert_eq!(root, expected);
    }

    #[test]
    fn invalid_hex_returns_error() {
        assert!(root_from_frontier_hex("zzzz").is_err());
    }

    #[test]
    fn truncated_data_returns_error() {
        assert!(root_from_frontier_hex("01").is_err());
    }

    /// Verify against the known-correct nc_root from zcash_client_backend at mainnet height 3245500.
    #[test]
    fn mainnet_3245500_matches_zcash_client_backend() {
        // orchardTree hex from lightwalletd GetTreeState at height 3245500
        let hex_str = "01ca2a21eba4c591869db2b8fdf4dd5ba493d6bfceac23be46ca558ffed0dc921d001f0001cff1b87edbd466dbba37844dc634e456af9650eff4707e38f3ac7abadd20811d0000015b2cefe8fb1ea9a719a7ca95121b2ed30d5232a11fb2147648d1a295b8eb433e014ba2c229d7a29d9eb8d037696bd2821bcceaeb6ed295428487333d3f12e791340000000001f3715072c78b20389957accac152bcea31bdf72570c7b27fe971bf73b6890a1701882a011843c0e6e4006e0a061222047775ccb2f5e560170807f466010d3ac41b000160ba626f9d0861510dd8bad09e1479eb74a93ed48cbbdea8dd14e1f63fd1123b00015fd78b01fa7f2d305ef8c6b968027c4020ba0ad8dd6a2d12218b9a9120747c2f01fb82740a3629216088191f9cd359c52a2f35b1c58f6cc905781bd9687b66ad3801eac2b89b3f966d833626434df98d553e000324bbafb8d6e1fe03b8d7f854cf2a00017c8ece2b2ab2355d809b58809b21c7a5e95cfc693cd689387f7533ec8749261e01cc2dcaa338b312112db04b435a706d63244dd435238f0aa1e9e1598d35470810012dcc4273c8a0ed2337ecf7879380a07e7d427c7f9d82e538002bd1442978402c01daf63debf5b40df902dae98dadc029f281474d190cddecef1b10653248a234150001e2bca6a8d987d668defba89dc082196a922634ed88e065c669e526bb8815ee1b000000000000";
        let root = root_from_frontier_hex(hex_str).unwrap();
        // This is the value produced by librustvoting::witness::extract_nc_root (uses zcash_client_backend)
        assert_eq!(
            hex::encode(root),
            "698e8409ae1d6b2e977ee5b8d37098f4fce2f07d5ac0b62269170b8cca077103",
        );
    }
}
