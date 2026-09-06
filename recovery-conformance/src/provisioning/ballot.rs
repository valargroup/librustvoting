//! The ballot this suite votes, and the round description built from it.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Notes the fixed voter wallet holds.
///
/// Asserted at provisioning rather than assumed: bundling is value-sensitive,
/// so rebalancing the wallet can change the layout and silently weaken the
/// multi-bundle invariants.
pub const EXPECTED_VOTER_NOTES: usize = 11;

/// Bundles those notes produce.
///
/// Notes pack five to a bundle (`BUNDLE_NOTE_SLOTS`) value-descending, so 11
/// fill 5/5/1. The privacy trim then tries to shed the smallest bundle down to
/// `DEFAULT_MAX_PRIVACY_BUNDLES` = 2, but may spend only 1% of selected value;
/// one note of a near-equal set is far outside that budget, so the trim stops
/// and all three survive. The third bundle is what gives failure-isolation a
/// bundle to spare.
pub const EXPECTED_BUNDLE_COUNT: usize = 3;

/// Failure isolation needs a bundle to spare: it crashes one bundle mid-proof
/// and asserts the others are untouched, which two bundles barely demonstrate.
/// Enforced at compile time so a wallet change cannot quietly weaken `E1`.
const _: () = assert!(EXPECTED_BUNDLE_COUNT > 2);

/// One selectable answer to a proposal.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoundOption {
    /// Zero-based, as the chain requires.
    pub index: u32,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One question on the ballot.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct RoundProposal {
    /// One-based, as the chain requires.
    pub id: u32,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Two to eight, as the chain requires.
    pub options: Vec<RoundOption>,
}

/// The `MsgCreateVotingSession` description `svoted` consumes.
///
/// Byte fields are hex, matching the CLI's own format. The snapshot fields are
/// not invented here: they must name a real published snapshot, or the round
/// would be unusable by any wallet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoundDescription {
    pub snapshot_height: u64,
    pub snapshot_blockhash: String,
    pub proposals_hash: String,
    pub vote_end_time: i64,
    pub nullifier_imt_root: String,
    pub nc_root: String,
    pub proposals: Vec<RoundProposal>,
}

/// The snapshot a round is anchored to.
///
/// Kept separate from the ballot shape because the ballot is ours to choose and
/// the snapshot is not: it has to match a published nullifier/commitment
/// snapshot for the round to be votable.
#[derive(Clone, Debug)]
pub struct SnapshotAnchor {
    pub height: u64,
    pub blockhash_hex: String,
    pub nullifier_imt_root_hex: String,
    pub nc_root_hex: String,
}

impl RoundDescription {
    /// Builds the description for a round with the suite's ballot shape.
    ///
    /// `proposals_hash` is computed here rather than supplied. The chain only
    /// checks that it is non-empty — it never recomputes it — but it feeds the
    /// Poseidon round-id derivation, so it must be *stable* even though it need
    /// not be canonical. Deriving it from the ballot means two runs asking for
    /// the same ballot cannot silently produce different round identities.
    pub fn new(anchor: &SnapshotAnchor, vote_end_time: i64, proposals: Vec<RoundProposal>) -> Self {
        let proposals_hash = proposals_hash(&proposals);
        Self {
            snapshot_height: anchor.height,
            snapshot_blockhash: anchor.blockhash_hex.clone(),
            proposals_hash,
            vote_end_time,
            nullifier_imt_root: anchor.nullifier_imt_root_hex.clone(),
            nc_root: anchor.nc_root_hex.clone(),
            proposals,
        }
    }

    /// Serializes to the JSON file `svoted tx vote create-voting-session` reads.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// The ballot this suite votes: three proposals with two, three, and four
/// options.
///
/// Differing option counts are deliberate. `num_options` rides on each vote and
/// bounds the choice, so a ballot where every proposal had the same width could
/// not catch a resume that reconstructed a vote against the wrong proposal's
/// bounds. Three proposals also make "vote work is proposal-primary" observable:
/// with one there is no ordering to get wrong.
pub fn suite_ballot() -> Vec<RoundProposal> {
    vec![
        RoundProposal {
            id: 1,
            title: "Recovery conformance: binary question".to_string(),
            description: Some("Two options, the narrowest ballot width.".to_string()),
            options: vec![option(0, "Yes"), option(1, "No")],
        },
        RoundProposal {
            id: 2,
            title: "Recovery conformance: ternary question".to_string(),
            description: Some("Three options, including an abstention.".to_string()),
            options: vec![option(0, "For"), option(1, "Against"), option(2, "Abstain")],
        },
        RoundProposal {
            id: 3,
            title: "Recovery conformance: four-way question".to_string(),
            description: Some("Four options, the widest ballot this suite uses.".to_string()),
            options: vec![
                option(0, "Option A"),
                option(1, "Option B"),
                option(2, "Option C"),
                option(3, "Option D"),
            ],
        },
    ]
}

fn option(index: u32, label: &str) -> RoundOption {
    RoundOption {
        index,
        label: label.to_string(),
        description: None,
    }
}

/// SHA-256 over a stable encoding of the ballot.
///
/// The encoding is this suite's own: `id:label,label|id:label...`. It is not a
/// consensus rule — the chain never recomputes this — so it only has to be
/// deterministic and to change whenever the ballot changes, so that a different
/// ballot cannot reuse a round identity.
fn proposals_hash(proposals: &[RoundProposal]) -> String {
    let mut encoded = String::new();
    for proposal in proposals {
        encoded.push_str(&proposal.id.to_string());
        encoded.push(':');
        for option in &proposal.options {
            encoded.push_str(&option.index.to_string());
            encoded.push('=');
            encoded.push_str(&option.label);
            encoded.push(',');
        }
        encoded.push('|');
    }
    let digest = Sha256::digest(encoded.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}
