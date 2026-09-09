//! The ballot one benchmark run votes, and where it comes from.
//!
//! Two sources, one shape. A synthetic ballot is described by a proposal count
//! and a cycle of option widths; an imported ballot is read from the round
//! export a vote manager produces (the repository's own `prop.json` is one).
//! Both produce the same [`Ballot`], so nothing downstream knows which was
//! used.
//!
//! # Identity translation
//!
//! The export identifies proposals and options by UUID. The SDK identifies a
//! proposal by a `u32` in `1..=50` and an option by its zero-based index
//! ([`MIN_PROPOSAL_ID`]/[`MAX_PROPOSAL_ID`], `MIN_OPTIONS`/`MAX_OPTIONS`), and
//! so does the chain's round description. The export's UUIDs are therefore
//! retained only in [`ProposalSource`], for the run manifest, and never reach
//! the SDK.

use anyhow::{bail, Context, Result};
use recovery_conformance::provisioning::{RoundOption, RoundProposal};
use serde::{Deserialize, Serialize};
use zcash_voting::session::Decision;
use zcash_voting::{BallotIntent, ProposalRosterEntry};

/// Lowest proposal id the SDK and the chain accept.
pub const MIN_PROPOSAL_ID: u32 = 1;

/// Highest proposal id the SDK accepts, and therefore the widest ballot a
/// benchmark can vote in one round.
///
/// Mirrors `zcash_voting::MAX_PROPOSAL_ID`, which is not public. A ballot above
/// this is rejected before anything is provisioned: the chain would accept the
/// round and `RoundBinding` would then refuse it, which spends a real round to
/// learn what a bounds check knows.
pub const MAX_PROPOSAL_ID: u32 = 50;

/// Fewest options a proposal may carry.
pub const MIN_OPTIONS: usize = 2;

/// Most options a proposal may carry.
pub const MAX_OPTIONS: usize = 8;

/// The default width cycle for a synthetic ballot.
///
/// Differing widths are deliberate, and inherited from the conformance suite's
/// reasoning: `num_options` rides on every vote and bounds its choice, so a
/// ballot where every proposal had the same width could not catch work
/// reconstructed against the wrong proposal's bounds.
pub const DEFAULT_OPTION_WIDTHS: &[usize] = &[2, 3, 4];

/// Where one proposal came from, for the run manifest.
///
/// Present so a run over an imported ballot can be traced back to the export
/// it replayed. Nothing here is used to drive the round.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProposalSource {
    /// The SDK/chain proposal id this export entry became.
    pub proposal_id: u32,
    /// The export's own identifier, when there was one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export_id: Option<String>,
    pub title: String,
    pub num_options: u32,
}

/// The ballot a run provisions and votes.
///
/// Holds the chain-facing proposals and the SDK-facing derivations together so
/// they cannot disagree: the roster, the intents, and the proposal ids are all
/// projections of the same `proposals` vector rather than three independently
/// maintained lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ballot {
    proposals: Vec<RoundProposal>,
    sources: Vec<ProposalSource>,
}

impl Ballot {
    /// A synthetic ballot of `count` proposals whose widths cycle `widths`.
    ///
    /// Rejects a count outside `1..=MAX_PROPOSAL_ID` and any width outside
    /// `MIN_OPTIONS..=MAX_OPTIONS`, both before a round is provisioned.
    pub fn synthetic(count: usize, widths: &[usize]) -> Result<Self> {
        if count == 0 || count > MAX_PROPOSAL_ID as usize {
            bail!("a ballot must have 1 to {MAX_PROPOSAL_ID} proposals, not {count}");
        }
        if widths.is_empty() {
            bail!("a synthetic ballot needs at least one option width");
        }
        for width in widths {
            if !(MIN_OPTIONS..=MAX_OPTIONS).contains(width) {
                bail!("a proposal takes {MIN_OPTIONS} to {MAX_OPTIONS} options, not {width}");
            }
        }

        let proposals: Vec<RoundProposal> = (0..count)
            .map(|index| {
                let width = widths[index % widths.len()];
                let id = u32::try_from(index).unwrap_or_default() + MIN_PROPOSAL_ID;
                RoundProposal {
                    id,
                    title: format!("Benchmark proposal {id}"),
                    description: Some(format!("Synthetic proposal with {width} options.")),
                    options: (0..width)
                        .map(|option| RoundOption {
                            index: u32::try_from(option).unwrap_or_default(),
                            label: format!("Option {}", option + 1),
                            description: None,
                        })
                        .collect(),
                }
            })
            .collect();

        Ok(Self::from_proposals(proposals, Vec::new()))
    }

    /// A ballot read from a vote manager's round export.
    ///
    /// Takes the proposals in export order and renumbers them `1..=n`; the
    /// export's own ids are kept in [`sources`](Self::sources) only. An export
    /// wider than [`MAX_PROPOSAL_ID`], or carrying a proposal outside the
    /// option bounds, is rejected here rather than by the chain.
    pub fn from_export(export: &[u8]) -> Result<Self> {
        let document: RoundExport =
            serde_json::from_slice(export).context("parsing the round export")?;
        let exported = document.round.proposals;
        if exported.is_empty() {
            bail!("the round export names no proposals");
        }
        if exported.len() > MAX_PROPOSAL_ID as usize {
            bail!(
                "the round export has {} proposals; the SDK accepts at most {MAX_PROPOSAL_ID}",
                exported.len()
            );
        }

        let mut proposals = Vec::with_capacity(exported.len());
        let mut sources = Vec::with_capacity(exported.len());
        for (index, proposal) in exported.into_iter().enumerate() {
            let id = u32::try_from(index).unwrap_or_default() + MIN_PROPOSAL_ID;
            let width = proposal.options.len();
            if !(MIN_OPTIONS..=MAX_OPTIONS).contains(&width) {
                bail!(
                    "exported proposal {:?} has {width} options; {MIN_OPTIONS} to {MAX_OPTIONS} \
                     are allowed",
                    proposal.title
                );
            }
            sources.push(ProposalSource {
                proposal_id: id,
                export_id: proposal.id.clone(),
                title: proposal.title.clone(),
                num_options: u32::try_from(width).unwrap_or_default(),
            });
            proposals.push(RoundProposal {
                id,
                title: proposal.title,
                description: proposal.description.filter(|text| !text.is_empty()),
                options: proposal
                    .options
                    .into_iter()
                    .enumerate()
                    .map(|(option, exported)| RoundOption {
                        index: u32::try_from(option).unwrap_or_default(),
                        label: exported.label,
                        description: exported.description.filter(|text| !text.is_empty()),
                    })
                    .collect(),
            });
        }

        Ok(Self::from_proposals(proposals, sources))
    }

    fn from_proposals(proposals: Vec<RoundProposal>, sources: Vec<ProposalSource>) -> Self {
        let sources = if sources.is_empty() {
            proposals
                .iter()
                .map(|proposal| ProposalSource {
                    proposal_id: proposal.id,
                    export_id: None,
                    title: proposal.title.clone(),
                    num_options: u32::try_from(proposal.options.len()).unwrap_or_default(),
                })
                .collect()
        } else {
            sources
        };
        Self { proposals, sources }
    }

    /// The chain-facing proposals, for the round description.
    pub fn proposals(&self) -> &[RoundProposal] {
        &self.proposals
    }

    /// Where each proposal came from, for the manifest.
    pub fn sources(&self) -> &[ProposalSource] {
        &self.sources
    }

    pub fn len(&self) -> usize {
        self.proposals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.proposals.is_empty()
    }

    /// The roster `RoundBinding` is built from.
    pub fn roster(&self) -> Vec<ProposalRosterEntry> {
        self.proposals
            .iter()
            .map(|proposal| ProposalRosterEntry {
                proposal_id: proposal.id,
                num_options: u32::try_from(proposal.options.len()).unwrap_or_default(),
            })
            .collect()
    }

    /// A terminal decision for every proposal, so no cast waits on a ballot.
    ///
    /// The choice is the proposal's position **modulo its own width**, not the
    /// position itself. Spreading choices across a ballot keeps every run from
    /// exercising only option zero; taking the remainder is what keeps the
    /// choice inside `num_options` once the ballot is wider than a proposal.
    /// A benchmark that voted an out-of-range choice would fail during
    /// commitment, after the round had already been spent.
    pub fn intents(&self) -> Vec<BallotIntent> {
        self.proposals
            .iter()
            .enumerate()
            .map(|(index, proposal)| BallotIntent {
                proposal_id: proposal.id,
                decision: Decision::Choice(
                    u32::try_from(index % proposal.options.len()).unwrap_or_default(),
                ),
            })
            .collect()
    }

    /// Proposal ids in ballot order, for `resume_plan`.
    pub fn proposal_ids(&self) -> Vec<u32> {
        self.proposals.iter().map(|proposal| proposal.id).collect()
    }
}

/// The subset of a vote manager's round export this benchmark reads.
///
/// Deliberately partial and lenient: the export carries scheduling, discussion
/// links, and display metadata a benchmark has no use for, and tolerating
/// unknown fields means a new key in the exporter does not break a replay.
#[derive(Debug, Deserialize)]
struct RoundExport {
    round: ExportedRound,
}

#[derive(Debug, Deserialize)]
struct ExportedRound {
    #[serde(default)]
    proposals: Vec<ExportedProposal>,
}

#[derive(Debug, Deserialize)]
struct ExportedProposal {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    options: Vec<ExportedOption>,
}

#[derive(Debug, Deserialize)]
struct ExportedOption {
    #[serde(default)]
    label: String,
    #[serde(default)]
    description: Option<String>,
}
