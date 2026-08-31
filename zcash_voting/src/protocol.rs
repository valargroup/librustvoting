use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};

/// Vote proof system selected for a voting round.
///
/// This version changes the delegation and vote proof statements, so it must
/// be fixed when the round is created and reused for every later operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VoteProtocol {
    /// Original 15-proposal proof system from `voting-circuits` 0.11.x.
    V0,
    /// 50-proposal proof system from `voting-circuits` 0.12.x.
    V1,
}

impl Default for VoteProtocol {
    fn default() -> Self {
        Self::V0
    }
}

impl VoteProtocol {
    pub const fn max_proposal_id(self) -> u32 {
        match self {
            Self::V0 => 15,
            Self::V1 => 50,
        }
    }

    pub const fn max_vote_batch_actions(self) -> usize {
        self.max_proposal_id() as usize
    }

    pub const fn initial_proposal_authority(self) -> u64 {
        match self {
            Self::V0 => u16::MAX as u64,
            Self::V1 => voting_circuits::MAX_PROPOSAL_AUTHORITY,
        }
    }
}

impl fmt::Display for VoteProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::V0 => f.write_str("v0"),
            Self::V1 => f.write_str("v1"),
        }
    }
}

impl FromStr for VoteProtocol {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "v0" => Ok(Self::V0),
            "v1" => Ok(Self::V1),
            _ => Err(format!("unsupported vote protocol {value}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_bounds_match_circuit_authority_masks() {
        assert_eq!(VoteProtocol::V0.max_proposal_id(), 15);
        assert_eq!(VoteProtocol::V0.initial_proposal_authority(), 65_535);
        assert_eq!(VoteProtocol::V1.max_proposal_id(), 50);
        assert_eq!(
            VoteProtocol::V1.initial_proposal_authority(),
            (1u64 << 51) - 1
        );
    }

    #[test]
    fn protocol_wire_names_are_stable() {
        assert_eq!(serde_json::to_string(&VoteProtocol::V0).unwrap(), "\"v0\"");
        assert_eq!(serde_json::to_string(&VoteProtocol::V1).unwrap(), "\"v1\"");
        assert_eq!("v0".parse(), Ok(VoteProtocol::V0));
        assert_eq!("v1".parse(), Ok(VoteProtocol::V1));
        assert!("v2".parse::<VoteProtocol>().is_err());
    }
}
