//! What a reopened sidecar must show, per stage.
//!
//! Every assertion here reads durable state through a freshly opened database,
//! because that is the only thing a crash leaves behind. Two rules shape them:
//!
//! - **The plan is the oracle.** `resume_plan` is a pure function of the
//!   sidecar, so what a round still owes is exactly what it returns. Raw rows
//!   are asserted only where the row *is* the invariant.
//! - **Normalization is lazy.** An abandoned `Submitting` row becomes
//!   `Recovering` inside the lifecycle's next admission, not when the database
//!   is opened. Asserting the raw state immediately after reopen therefore
//!   tests nothing about recovery, and an earlier version of this suite nearly
//!   reported that as a violation.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};

use zcash_voting::round::VotingDb;
use zcash_voting::session::{NextStep, RoundPlan};

use crate::stages::CrashStage;

/// Durable facts about one round, read straight from the sidecar.
///
/// Deliberately a snapshot of rows rather than of the SDK's projections: these
/// are the places an invariant lives, and reading them directly means a
/// projection bug cannot hide one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurableSnapshot {
    /// One row per chain submission, identity included.
    ///
    /// Identity is what makes "no second POST" checkable. Counting alone
    /// cannot distinguish a generation that retried from a second generation
    /// built to spend the same notes again; the digest can.
    pub submissions: Vec<Submission>,
    pub bundles: i64,
    pub proofs: i64,
    pub votes: i64,
    pub helper_share_plans: i64,
    pub share_delegations: i64,
    /// Whether any bundle has a durable PCZT sighash.
    /// Helpers durably journaled as attempted but unresolved.
    ///
    /// Written before any byte is sent, which is what makes an interrupted
    /// attempt recoverable rather than invisible: on restart the helper is
    /// known to have been tried with an unknown outcome, so it is polled rather
    /// than either re-sent blindly or written off as never contacted.
    pub attempting_urls: usize,
    /// Helpers that definitely accepted a share.
    pub accepted_urls: usize,
    /// Shares durably confirmed by the helper quorum.
    ///
    /// Completion, as distinct from placement: a share confirms whether its
    /// acceptance was recorded as definite or merely as outcome-unknown.
    pub confirmed_shares: i64,
    pub pczt_persisted: bool,
    /// Whether a cached vote-commitment tree exists.
    pub cached_tree: bool,
    /// Per-share helper placement, ordered by the share's durable identity.
    ///
    /// Deliberately not folded into the counts above. `attempting_urls` says
    /// how many shares have an unresolved attempt; this says which helpers
    /// hold which share, which is what a fleet scenario asserts about.
    pub deliveries: Vec<ShareDelivery>,
}

/// One durable chain submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Submission {
    pub kind: String,
    pub bundle_index: i64,
    pub proposal_id: Option<i64>,
    /// Hex of the generation digest: the identity of the transaction built.
    pub generation_digest: String,
    pub state: String,
    pub has_candidate_hash: bool,
    pub has_confirmed_hash: bool,
    pub confirmation_source: Option<String>,
    pub reservations: i64,
}

impl Submission {
    /// What the submission is for, ignoring which generation served it.
    pub fn target(&self) -> (String, i64, Option<i64>) {
        (self.kind.clone(), self.bundle_index, self.proposal_id)
    }
}

/// Where one share stands with each helper it has met.
///
/// The counts beside this in [`DurableSnapshot`] answer "did anything happen";
/// this answers "to whom", which is the only form the multi-URL invariants can
/// be stated in. Driven against one helper the two are interchangeable, which
/// is exactly why a one-URL suite could not see any of those rules.
///
/// The three sets are disjoint by construction in the SDK, with acceptance
/// taking precedence over an unknown outcome and an unknown outcome over an
/// attempt still in flight.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ShareDelivery {
    pub bundle_index: i64,
    pub proposal_id: i64,
    pub share_index: i64,
    /// Helpers durably reserved before dispatch whose outcome is unresolved.
    pub attempting: BTreeSet<String>,
    /// Helpers that definitely accepted. Never downgraded once written.
    pub sent: BTreeSet<String>,
    /// Helpers that completed a POST whose outcome could not be learned.
    pub ambiguous: BTreeSet<String>,
    pub confirmed: bool,
}

impl ShareDelivery {
    /// The share's durable identity.
    pub fn key(&self) -> (i64, i64, i64) {
        (self.bundle_index, self.proposal_id, self.share_index)
    }

    /// Helpers this share has been placed with or offered to, in any state.
    pub fn touched(&self) -> BTreeSet<String> {
        self.attempting
            .union(&self.sent)
            .chain(self.ambiguous.iter())
            .cloned()
            .collect()
    }
}

/// Reads every share's helper placement from the sidecar.
///
/// Ordered by the share's durable identity so two snapshots of the same round
/// compare positionally, and so a diff between a crash and its resume reads as
/// a list of shares rather than as a reordering.
fn read_deliveries(connection: &rusqlite::Connection) -> Result<Vec<ShareDelivery>> {
    let mut statement = connection
        .prepare(
            "select bundle_index, proposal_id, share_index,
                    attempting_urls, sent_to_urls, ambiguous_urls, confirmed
             from share_delegations
             order by bundle_index, proposal_id, share_index",
        )
        .context("querying share_delegations")?;
    let deliveries = statement
        .query_map([], |row| {
            Ok(ShareDelivery {
                bundle_index: row.get(0)?,
                proposal_id: row.get(1)?,
                share_index: row.get(2)?,
                attempting: url_set(row.get(3)?),
                sent: url_set(row.get(4)?),
                ambiguous: url_set(row.get(5)?),
                confirmed: row.get::<_, i64>(6)? == 1,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(deliveries)
}

/// Reads a helper URL list from its stored JSON array.
///
/// A missing, empty, or unparseable column is an empty set rather than an
/// error: these columns are written as JSON arrays and start out absent, and a
/// share that has met no helper is a normal state rather than a fault.
fn url_set(raw: Option<String>) -> BTreeSet<String> {
    raw.and_then(|value| serde_json::from_str::<Vec<String>>(&value).ok())
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// The comparable shape of a finished round. See [`DurableSnapshot::shape`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundShape {
    submissions: BTreeMap<(String, i64, Option<i64>), String>,
    bundles: i64,
    proofs: i64,
    votes: i64,
    helper_share_plans: i64,
    share_delegations: i64,
    confirmed_shares: i64,
    pczt_persisted: bool,
}

impl DurableSnapshot {
    /// Reads the snapshot from a sidecar path, opening its own connection.
    pub fn read(sidecar: &std::path::Path) -> Result<Self> {
        let connection =
            rusqlite::Connection::open(sidecar).context("opening the sidecar for inspection")?;
        let mut statement = connection
            .prepare(
                "select kind, bundle_index, proposal_id, hex(generation_digest), state,
                        candidate_transaction_hash is not null,
                        confirmed_transaction_hash is not null,
                        confirmation_source, committed_post_reservations
                 from chain_submissions
                 order by kind, bundle_index, proposal_id, hex(generation_digest)",
            )
            .context("querying chain_submissions")?;
        let submissions = statement
            .query_map([], |row| {
                Ok(Submission {
                    kind: row.get(0)?,
                    bundle_index: row.get(1)?,
                    proposal_id: row.get(2)?,
                    generation_digest: row.get(3)?,
                    state: row.get(4)?,
                    has_candidate_hash: row.get(5)?,
                    has_confirmed_hash: row.get(6)?,
                    confirmation_source: row.get(7)?,
                    reservations: row.get(8)?,
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        drop(statement);

        let count = |table: &str| -> i64 {
            connection
                .query_row(&format!("select count(*) from {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap_or(0)
        };
        Ok(Self {
            submissions,
            bundles: count("bundles"),
            proofs: count("proofs"),
            votes: count("votes"),
            helper_share_plans: count("helper_share_plans"),
            share_delegations: count("share_delegations"),
            attempting_urls: connection
                .query_row(
                    "select count(*) from share_delegations
                     where attempting_urls is not null and attempting_urls != '' and attempting_urls != '[]'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0) as usize,
            accepted_urls: connection
                .query_row(
                    "select count(*) from share_delegations
                     where sent_to_urls is not null and sent_to_urls != '' and sent_to_urls != '[]'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0) as usize,
            confirmed_shares: connection
                .query_row(
                    "select count(*) from share_delegations where confirmed = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0),
            pczt_persisted: connection
                .query_row(
                    "select count(*) from bundles where pczt_sighash is not null",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0,
            cached_tree: count("cached_tree_state") > 0,
            deliveries: read_deliveries(&connection)?,
        })
    }

    /// Total committed POST reservations across every submission.
    ///
    /// Monotonic by trigger, and the number that proves a resumed round did not
    /// quietly send a second transaction.
    pub fn total_reservations(&self) -> i64 {
        self.submissions
            .iter()
            .map(|submission| submission.reservations)
            .sum()
    }

    /// Submission states, for stage assertions that name one.
    pub fn states(&self) -> Vec<&str> {
        self.submissions
            .iter()
            .map(|submission| submission.state.as_str())
            .collect()
    }

    /// The shape a resumed round must share with the uncrashed control.
    ///
    /// Comparing state names alone accepts substantial corruption: a round can
    /// lose votes, plans or share rows and still report twelve submissions all
    /// named `confirmed`. This keys each submission by what it is rather than
    /// by list position, and carries the durable counts alongside.
    ///
    /// What it deliberately omits is anything that legitimately differs
    /// between two distinct rounds: identity keys, generation digests, tree
    /// positions, and everything that records *how* a submission confirmed
    /// rather than that it did. A crashed round may confirm by tree where the
    /// control confirmed by hash, which is recovery working, not a divergence.
    ///
    /// That covers the confirmed transaction hash as well as
    /// `confirmation_source`. The two say the same thing — the schema forbids
    /// a tree confirmation from carrying a hash — so keeping the hash would
    /// reintroduce the very difference this excludes, and fail every stage
    /// whose recovery went through the tree.
    ///
    /// It covers helper placement for the same reason. A crash around a share
    /// POST leaves that helper outcome-unknown rather than a definite
    /// acceptance, which the helper specification treats as poll-only and not
    /// a placement — so `sent_to_urls` records how a share was delivered, not
    /// whether it arrived. Shares are compared by how many are *confirmed*,
    /// which is the completion the round actually owes.
    pub fn shape(&self) -> RoundShape {
        RoundShape {
            submissions: self
                .submissions
                .iter()
                .map(|submission| {
                    (
                        (
                            submission.kind.clone(),
                            submission.bundle_index,
                            submission.proposal_id,
                        ),
                        submission.state.clone(),
                    )
                })
                .collect(),
            bundles: self.bundles,
            proofs: self.proofs,
            votes: self.votes,
            helper_share_plans: self.helper_share_plans,
            share_delegations: self.share_delegations,
            confirmed_shares: self.confirmed_shares,
            pczt_persisted: self.pczt_persisted,
        }
    }

    /// Submissions keyed by generation, for cross-snapshot comparison.
    pub fn by_generation(&self) -> BTreeMap<(String, i64, Option<i64>, String), &Submission> {
        self.submissions
            .iter()
            .map(|submission| {
                (
                    (
                        submission.kind.clone(),
                        submission.bundle_index,
                        submission.proposal_id,
                        submission.generation_digest.clone(),
                    ),
                    submission,
                )
            })
            .collect()
    }
}

/// Opens a sidecar and returns the plan twice, proving determinism.
///
/// Every other assertion rests on the plan being a function of durable state,
/// so it is checked rather than assumed.
pub fn deterministic_plan(
    sidecar: &std::path::Path,
    account_uuid: &str,
    round_id: &str,
    proposal_ids: &[u32],
) -> Result<RoundPlan> {
    let database = VotingDb::open_path(sidecar)
        .map_err(|error| anyhow::anyhow!("reopening the sidecar: {error:?}"))?;
    database.set_wallet_id(account_uuid);

    let first = zcash_voting::session::resume_plan(&database, round_id, proposal_ids)
        .map_err(|error| anyhow::anyhow!("planning: {error:?}"))?;
    let second = zcash_voting::session::resume_plan(&database, round_id, proposal_ids)
        .map_err(|error| anyhow::anyhow!("re-planning: {error:?}"))?;
    anyhow::ensure!(
        first.next_steps == second.next_steps,
        "A1 VIOLATED: two plans over the same durable state disagree:\n  {:?}\n  {:?}",
        first.next_steps,
        second.next_steps
    );
    Ok(first)
}

/// Asserts the state a crash at `stage` must leave, for the named bundle.
///
/// Only facts the stage actually determines are asserted. A stage that shares a
/// durable boundary with its neighbour asserts what they share, rather than
/// inventing a distinction the rows do not carry.
pub fn assert_stage_state(
    stage: CrashStage,
    plan: &RoundPlan,
    snapshot: &DurableSnapshot,
    bundle: u32,
) -> Result<()> {
    use CrashStage as S;
    match stage {
        // Nothing durable has been added, so the bundle still owes an ordinary
        // delegation.
        S::BeforeDelegation | S::AfterNoteSelection => {
            require_step(
                plan,
                &NextStep::Delegate {
                    bundle_index: bundle,
                },
                stage,
            )?;
        }
        // The PCZT is write-once. Resume must reuse it, and the observable
        // consequence is that the bundle is still delegating rather than
        // starting over from setup.
        S::AfterPczt => {
            anyhow::ensure!(
                snapshot.pczt_persisted,
                "{stage}: no durable PCZT, so write-once reuse cannot be exercised"
            );
            require_step(
                plan,
                &NextStep::Delegate {
                    bundle_index: bundle,
                },
                stage,
            )?;
        }
        // ZKP #1 is durable. Resume must reuse the proof rather than re-enter
        // PIR and prove again.
        S::AfterProof | S::AfterSigning => {
            anyhow::ensure!(
                snapshot.proofs > 0,
                "{stage}: no durable proof, so proof reuse cannot be exercised"
            );
            anyhow::ensure!(plan.next_steps.iter().any(|step| matches!(step, NextStep::CastVote { bundle_index, .. } if *bundle_index == bundle)), "{stage}: prepared delegation must resume the combined cast");
        }
        // The sharp case: bytes provably never left, yet the reservation must
        // survive as in-flight work. Re-delegating would build a second
        // transaction spending the same notes.
        // The sharp cases. `BeforeBroadcast` is the conservative one: the bytes
        // provably never left, yet the reservation must survive. The spec's
        // "normalize to Recovering on restart" happens lazily, inside the
        // lifecycle's next admission, so what is checkable at reopen is the
        // half that matters for safety — the row is **still there**. A row that
        // vanished would let the next pass reserve a fresh first attempt and
        // build a second transaction over the same notes.
        S::BeforeBroadcast => {
            anyhow::ensure!(
                snapshot
                    .submissions
                    .iter()
                    .any(|submission| submission.kind == "delegate_and_cast_vote_batch" && submission.state == "submitting"),
                "B1 VIOLATED: the abandoned reservation is gone after reopen (states {:?});                  a restarted process cannot prove the bytes never left, so the row must                  survive as in-flight work rather than be discarded",
                snapshot.states()
            );
            anyhow::ensure!(plan.next_steps.iter().any(|step| matches!(step, NextStep::AdvanceVoteBatch { bundle_index, .. } if *bundle_index == bundle)), "{stage}: combined recovery must advance the complete batch");
            forbid_step(
                plan,
                &NextStep::Delegate {
                    bundle_index: bundle,
                },
                stage,
                "re-delegating would spend the bundle's notes a second time",
            )?;
        }
        S::AfterBroadcastUnread | S::AfterBroadcastRead | S::AfterTracking => {
            anyhow::ensure!(plan.next_steps.iter().any(|step| matches!(step, NextStep::AdvanceVoteBatch { bundle_index, .. } if *bundle_index == bundle)), "{stage}: combined recovery must advance the complete batch");
            forbid_step(
                plan,
                &NextStep::Delegate {
                    bundle_index: bundle,
                },
                stage,
                "re-delegating would spend the bundle's notes a second time",
            )?;
            anyhow::ensure!(
                !snapshot.submissions.is_empty(),
                "{stage}: no durable submission row, so the dispatch left no evidence"
            );
        }
        // The delegation is confirmed and the vote has not been written.
        S::BeforeCast | S::AfterTreeSync | S::AfterVoteProof => {
            anyhow::ensure!(
                plan.next_steps
                    .iter()
                    .any(|step| matches!(step, NextStep::CastVote { .. })),
                "{stage}: no cast is planned, so the vote work was lost rather than retried"
            );
        }
        // The vote is committed with no POST reserved, so it must reconcile
        // rather than be recast.
        S::AfterVoteCommit | S::AfterHelperPlans => {
            anyhow::ensure!(
                snapshot.votes > 0,
                "{stage}: no durable vote, so the commit was lost"
            );
            anyhow::ensure!(
                plan.next_steps.iter().any(|step| matches!(
                    step,
                    NextStep::AdvanceVote { .. } | NextStep::AdvanceVoteBatch { .. }
                )),
                "{stage}: a committed vote is not planned for reconciliation"
            );
            // What separates the two boundaries. Without this the stage asserts
            // nothing `after-helper-plans` does not already cover, and a crash
            // that landed on the wrong side of the plan write would pass.
            if stage == S::AfterVoteCommit {
                anyhow::ensure!(
                    snapshot.helper_share_plans == 0,
                    "{stage}: helper plans are already durable, so the crash landed \
                     after the plan write rather than between it and the commit"
                );
                // The ordering rule read from the earlier side: plans precede
                // the broadcast, so a vote with no plan yet must have no chain
                // row either.
                anyhow::ensure!(
                    snapshot
                        .submissions
                        .iter()
                        .all(|submission| submission.kind == "delegation"),
                    "{stage}: a vote reached the chain before its helper plans were \
                     durable, which the ordering rule forbids"
                );
            }
        }
        S::BeforeVoteBroadcast | S::AfterVoteBroadcast | S::AfterVoteConfirmed => {
            anyhow::ensure!(
                snapshot.votes > 0,
                "{stage}: no durable vote behind the submission"
            );
        }
        // The helper is durably journaled before any byte is sent, which is
        // what makes an interrupted attempt recoverable rather than invisible.
        // The helper was journaled before any byte was sent, so a process
        // killed around the POST leaves durable evidence that it was tried and
        // its answer is unknown. Without that marker, recovery would either
        // re-send blindly or treat the helper as never contacted.
        S::BeforeSharePost | S::AfterSharePost => {
            anyhow::ensure!(
                snapshot.share_delegations > 0,
                "{stage}: no share_delegations row, so the attempt left no durable marker"
            );
            anyhow::ensure!(
                snapshot.attempting_urls > 0,
                "D1 VIOLATED: {stage} left no helper in `attempting_urls`; an attempt whose \
                 outcome is unknown must be recorded as attempted, or resume cannot tell it \
                 apart from one never made"
            );
        }
        // A definite acceptance is durable and must not be downgraded later.
        S::AfterShareAccepted => {
            anyhow::ensure!(
                snapshot.share_delegations > 0,
                "{stage}: no share_delegations row, so the attempt left no durable marker"
            );
            anyhow::ensure!(
                snapshot.accepted_urls > 0,
                "D2 VIOLATED: {stage} recorded no definite acceptance in `sent_to_urls`"
            );
        }
    }
    Ok(())
}

/// A committed vote must never be broadcast before its helper plans exist.
///
/// The ordering is what makes a confirmed-vote-without-a-plan unreachable, and
/// it is checkable from rows alone: if a vote submission exists, a plan must.
pub fn assert_plans_precede_broadcast(snapshot: &DurableSnapshot) -> Result<()> {
    let vote_submission = snapshot.submissions.iter().any(|submission| {
        submission.kind == "vote"
            || submission.kind == "vote_batch"
            || submission.kind == "delegate_and_cast_vote_batch"
    });
    if vote_submission {
        anyhow::ensure!(
            snapshot.helper_share_plans > 0,
            "C5 VIOLATED: a vote submission exists with no durable helper plan, so a \
             confirmation could leave a vote whose shares can never be delivered"
        );
    }
    Ok(())
}

/// What a confirmed delegation's confirmation rests on: `hash` or `tree`.
///
/// The distinction is the whole point of exact-tree recovery. A submission
/// whose response was never read has no candidate hash to poll, so the only way
/// it can confirm is by scanning the commitment tree for its generation. Seeing
/// `tree` here is what separates "recovery worked" from "the wallet happened to
/// still hold a usable hash".
pub fn confirmation_source(sidecar: &std::path::Path) -> Result<Option<String>> {
    let connection = rusqlite::Connection::open(sidecar).context("opening the sidecar")?;
    Ok(connection
        .query_row(
            "select confirmation_source from chain_submissions
             where kind IN ('delegation','delegate_and_cast_vote_batch') and confirmation_source is not null limit 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok())
}

/// A hashless dispatch must confirm, by one of the two routes open to it.
///
/// It used to require `tree`, on the reasoning that a submission whose response
/// was never read has no candidate hash to poll, so only an exact-tree scan can
/// resolve it. The first half is right and the conclusion is not: the
/// specification also allows a bounded *same-generation* retry from
/// `Recovering`, and states that "a canonical accepted hash from a hashless
/// retry resumes `Tracking`". Re-POSTing the identical transaction and being
/// handed back its hash is therefore a legal resolution, and on staging it is
/// the one that usually wins.
///
/// Requiring `tree` looked sound for a long time because the crash boundary was
/// wrong: aborting only after the full HTTP response had been read gave the
/// chain time to include the transaction, so the tree route won the race. Once
/// the crash moved to the real dispatch boundary the retry began winning
/// instead — and waiting for block inclusion before resuming does not change
/// that, so the route is the SDK's choice rather than a function of what the
/// chain holds.
///
/// What must hold either way is that the round confirmed *this* generation and
/// built no other, which [`assert_no_second_generation`] carries. This is
/// deliberately the weaker claim: the route is reported for every run so a
/// change in it is visible, but a suite cannot assert a choice the
/// specification leaves open.
pub fn assert_confirmed_by_a_legal_route(source: Option<&str>) -> Result<()> {
    let source = source.context(
        "B3 VIOLATED: the delegation confirmed without recording what the confirmation \
         rested on",
    )?;
    anyhow::ensure!(
        source.eq_ignore_ascii_case("tree") || source.eq_ignore_ascii_case("hash"),
        "B3 VIOLATED: a submission confirmed from {source:?}, which is neither of the two \
         routes a hashless dispatch may take"
    );
    Ok(())
}

/// The transaction hash a submission finally confirmed on.
pub fn confirmed_transaction_hash(sidecar: &std::path::Path) -> Result<Option<String>> {
    let connection = rusqlite::Connection::open(sidecar).context("opening the sidecar")?;
    let hash = connection
        .query_row(
            "select confirmed_transaction_hash from chain_submissions
             where kind IN ('delegation','delegate_and_cast_vote_batch') and confirmed_transaction_hash is not null limit 1",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok();
    Ok(hash)
}

/// The transaction the crashed child saw the chain accept, if it captured one.
///
/// Only the `after-*-read` stages record a response body. Parsing the hash out
/// of it is what turns "a transaction exists" into "*this* transaction
/// exists" — the difference between inferring no-double-spend from eventual
/// confirmation and demonstrating it by identity.
pub fn dispatched_transaction_hash(response_body: &str) -> Option<String> {
    let parsed: serde_json::Value = serde_json::from_str(response_body).ok()?;
    for key in ["txhash", "tx_hash", "hash"] {
        if let Some(hash) = parsed.get(key).and_then(serde_json::Value::as_str) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// The resumed round confirmed the transaction the crash already dispatched.
///
/// This is the direct form of "no second POST": not that exactly one
/// transaction eventually confirmed, but that the one that confirmed is the one
/// the killed process had already put on the wire. A round that had quietly
/// sent a replacement would confirm a different hash while looking equally
/// healthy.
pub fn assert_recovered_the_same_transaction(
    dispatched: &str,
    confirmed: Option<&str>,
    source: Option<&str>,
) -> Result<()> {
    match source {
        // An exact-tree scan matches the generation's own commitments rather
        // than a transaction id, and the schema forbids a hash in this case:
        // `CHECK (confirmation_source != 'tree' OR confirmed_transaction_hash
        // IS NULL)`. Identity is established by the scan having succeeded, and
        // non-duplication by the reservation count. Demanding a hash here would
        // assert something the database rules out.
        Some(source) if source.eq_ignore_ascii_case("tree") => {
            anyhow::ensure!(
                confirmed.is_none(),
                "a tree-confirmed submission carries no transaction hash by construction, \
                 but this one has {confirmed:?}"
            );
            Ok(())
        }
        // A hash confirmation can be compared directly, and this is the sharp
        // form of "no second POST": a round that quietly sent a replacement
        // would confirm a different hash while looking equally healthy.
        Some(_) => {
            let confirmed = confirmed
                .context("B2 VIOLATED: a hash-confirmed submission recorded no transaction hash")?;
            anyhow::ensure!(
                confirmed.eq_ignore_ascii_case(dispatched),
                "B2 VIOLATED: the round confirmed transaction {confirmed} but the crash had \
                 already dispatched {dispatched}; a different hash means a second transaction \
                 was sent"
            );
            Ok(())
        }
        None => anyhow::bail!(
            "B3 VIOLATED: the round never confirmed a delegation, so the dispatched \
             transaction was neither recovered nor ruled out"
        ),
    }
}

/// Reservations never decrease, and no generation gains a second one silently.
pub fn assert_reservations_monotonic(
    before: &DurableSnapshot,
    after: &DurableSnapshot,
) -> Result<()> {
    anyhow::ensure!(
        after.total_reservations() >= before.total_reservations(),
        "B5 VIOLATED: committed_post_reservations decreased from {} to {}",
        before.total_reservations(),
        after.total_reservations()
    );
    Ok(())
}

/// Requires that recovery never built a second transaction for the same work.
///
/// Monotonic totals cannot show this. A duplicate POST also raises the count,
/// so "the number went up" is consistent both with a legitimate same-generation
/// retry and with a second transaction spending the same notes — the failure
/// this suite exists to catch. Generation identity separates them: the digest
/// is immutable per submission, so a second transaction for the same target can
/// only appear as a *new* generation.
///
/// Two things are therefore required of every target that already had a
/// submission when the process died:
///
/// - it still has exactly one generation, so nothing was rebuilt; and
/// - that generation's digest is unchanged, so nothing was rewritten in place.
pub fn assert_no_second_generation(
    before: &DurableSnapshot,
    after: &DurableSnapshot,
) -> Result<()> {
    for submission in &before.submissions {
        let target = submission.target();
        let generations: Vec<&Submission> = after
            .submissions
            .iter()
            .filter(|candidate| candidate.target() == target)
            .collect();
        anyhow::ensure!(
            generations.len() == 1,
            "B2/B6 VIOLATED: {} bundle {} proposal {:?} has {} generations after resume, so a \
             second transaction was built for work that already had one: {:?}",
            submission.kind,
            submission.bundle_index,
            submission.proposal_id,
            generations.len(),
            generations
                .iter()
                .map(|generation| generation.generation_digest.as_str())
                .collect::<Vec<_>>()
        );
        anyhow::ensure!(
            generations[0].generation_digest == submission.generation_digest,
            "B6 VIOLATED: {} bundle {} changed generation digest across resume, {} -> {}",
            submission.kind,
            submission.bundle_index,
            submission.generation_digest,
            generations[0].generation_digest
        );
    }
    Ok(())
}

/// Requires that bundles the crash did not touch reserved nothing further.
///
/// The crashed bundle may legitimately reserve again — an abandoned
/// `Submitting` row normalizes to `Recovering`, and recovery is allowed a
/// bounded same-generation retry. No such licence extends to the other
/// bundles: they were idle across the crash, so any movement in their
/// reservation counts is a POST nothing asked for. This is the exact-count
/// half of the no-second-POST claim, on the submissions where an exact count
/// is actually determined.
pub fn assert_untouched_bundles_did_not_reserve(
    before: &DurableSnapshot,
    after: &DurableSnapshot,
    crashed_bundle: u32,
) -> Result<()> {
    let settled = after.by_generation();
    for (key, submission) in before.by_generation() {
        if submission.bundle_index == i64::from(crashed_bundle) {
            continue;
        }
        let Some(after_row) = settled.get(&key) else {
            continue;
        };
        anyhow::ensure!(
            after_row.reservations == submission.reservations,
            "B2 VIOLATED: uncrashed bundle {} ({}) reserved a further POST across resume, {} -> {}",
            submission.bundle_index,
            submission.kind,
            submission.reservations,
            after_row.reservations
        );
    }
    Ok(())
}

/// Requires the resumed round to match the control in shape, not just in
/// state names.
///
/// See [`DurableSnapshot::shape`] for what is compared and what is
/// deliberately allowed to differ between two distinct rounds.
pub fn assert_matches_control(terminal: &DurableSnapshot, control: &DurableSnapshot) -> Result<()> {
    anyhow::ensure!(
        terminal.shape() == control.shape(),
        "A3 VIOLATED: the resumed round does not match the uncrashed control.\n  \
         resumed: {:?}\n  control: {:?}",
        terminal.shape(),
        control.shape()
    );
    Ok(())
}

/// Terminal rows are immutable: a confirmed submission stays exactly as it was.
pub fn assert_terminal_rows_unchanged(
    before: &DurableSnapshot,
    after: &DurableSnapshot,
) -> Result<()> {
    let settled = after.by_generation();
    for (key, submission) in before.by_generation() {
        if !matches!(
            submission.state.as_str(),
            "confirmed" | "rejected" | "submitted_without_hash"
        ) {
            continue;
        }
        let after_row = settled.get(&key).with_context(|| {
            format!(
                "B4 VIOLATED: terminal {} row for bundle {} generation {} vanished across resume",
                submission.kind, submission.bundle_index, submission.generation_digest
            )
        })?;
        anyhow::ensure!(
            *after_row == submission,
            "B4 VIOLATED: terminal {} row for bundle {} changed across resume: {:?} -> {:?}",
            submission.kind,
            submission.bundle_index,
            submission,
            after_row
        );
    }
    Ok(())
}

/// Bundles other than the crashed one keep their pending work.
pub fn assert_other_bundles_untouched(
    plan: &RoundPlan,
    crashed: u32,
    bundle_count: u32,
) -> Result<()> {
    for bundle in 0..bundle_count {
        if bundle == crashed {
            continue;
        }
        anyhow::ensure!(
            plan.next_steps
                .iter()
                .any(|step| step_bundle(step) == Some(bundle)),
            "E1 VIOLATED: bundle {bundle} lost all pending work after a crash on bundle {crashed}"
        );
    }
    Ok(())
}

/// A second resume must find no *foreground* work left to do.
///
/// `ConfirmShare` steps are excluded, and that is not a loosening. A round that
/// ends in `BackgroundShareWorkOnly` has, by the orchestration specification,
/// finished everything the foreground flow owns: only polls for shares a helper
/// has already accepted remain, and the host's background tracking closes them
/// on its own timer. Demanding an empty plan would require the matrix to wait
/// out that timer to call a converged round converged — and would report the
/// SDK's designed hand-off as an idempotence violation, which is exactly what
/// an earlier version of this assertion did for every vote stage.
pub fn assert_idempotent(plan: &RoundPlan) -> Result<()> {
    let foreground: Vec<&NextStep> = plan
        .next_steps
        .iter()
        .filter(|step| !matches!(step, NextStep::ConfirmShare { .. }))
        .collect();
    anyhow::ensure!(
        foreground.is_empty(),
        "A4 VIOLATED: a resumed round that reached quiescence still plans foreground work: {foreground:?}"
    );
    Ok(())
}

fn require_step(plan: &RoundPlan, wanted: &NextStep, stage: CrashStage) -> Result<()> {
    anyhow::ensure!(
        plan.next_steps.contains(wanted),
        "{stage}: expected {wanted:?} in the resumed plan, got {:?}",
        plan.next_steps
    );
    Ok(())
}

fn forbid_step(plan: &RoundPlan, forbidden: &NextStep, stage: CrashStage, why: &str) -> Result<()> {
    anyhow::ensure!(
        !plan.next_steps.contains(forbidden),
        "{stage}: plan contains {forbidden:?}; {why}"
    );
    Ok(())
}

/// The bundle a step belongs to.
fn step_bundle(step: &NextStep) -> Option<u32> {
    match step {
        NextStep::Delegate { bundle_index }
        | NextStep::AdvanceDelegation { bundle_index }
        | NextStep::AdvanceImportedDelegation { bundle_index }
        | NextStep::CastVote { bundle_index, .. }
        | NextStep::AdvanceVote { bundle_index, .. }
        | NextStep::AdvanceVoteBatch { bundle_index, .. }
        | NextStep::SubmitShares { bundle_index, .. }
        | NextStep::ConfirmShare { bundle_index, .. } => Some(*bundle_index),
        _ => None,
    }
}

/// A definite acceptance is durable and must never be taken back.
///
/// `sent_to_urls` records that a helper *said it had the share*. Nothing later
/// — a restart, a helper going offline, a retry that fails — can make that
/// untrue, so the set may only grow. This is the multi-URL form of `B4`: the
/// crash matrix asserts terminal submission rows are byte-identical across a
/// resume, and this asserts the same thing about helper placement.
///
/// The case it exists for is the fleet flip. When the helpers that accepted a
/// share become unreachable and a different half comes up, the temptation for a
/// buggy implementation is to treat "cannot reach it now" as "never had it",
/// which would both lose the placement and licence a re-send.
pub fn assert_acceptances_never_downgraded(
    before: &DurableSnapshot,
    after: &DurableSnapshot,
) -> Result<()> {
    let later: BTreeMap<_, _> = after
        .deliveries
        .iter()
        .map(|delivery| (delivery.key(), delivery))
        .collect();
    for delivery in &before.deliveries {
        let Some(now) = later.get(&delivery.key()) else {
            anyhow::bail!(
                "share {:?} lost its durable row entirely, so its acceptances went with it",
                delivery.key()
            );
        };
        let lost: Vec<_> = delivery.sent.difference(&now.sent).cloned().collect();
        anyhow::ensure!(
            lost.is_empty(),
            "D2 VIOLATED: share {:?} no longer records {lost:?} as having accepted it; a \
             definite acceptance is durable and may not be downgraded",
            delivery.key()
        );
    }
    Ok(())
}

/// Every share must end the round durably confirmed.
///
/// The completion a round actually owes, and the one a fleet cannot change:
/// however helpers came and went, every share must end held and acknowledged.
///
/// # Why this is not a placement-target assertion
///
/// It was, and the first live run showed the target is not owed at the end of a
/// round. A share whose fan-out reached fewer helpers than `target_count` still
/// **confirms**, and once it is confirmed nothing repairs the shortfall:
/// background tracking walks unconfirmed shares only, so it correctly reports
/// `NothingToTrack` and contacts nobody. The observed case was a share
/// confirmed with two definite acceptances against a target of five.
///
/// That is a statement about the protocol, not a defect this suite can call.
/// `target_count` governs initial placement; confirmation is completion. The
/// gap between them — a share can finish a round held by fewer helpers than the
/// policy aimed for, with no mechanism to notice — is recorded in the README
/// under "Not asserted yet", because it is worth a decision rather than a
/// silent assertion either way.
pub fn assert_every_share_is_confirmed(snapshot: &DurableSnapshot) -> Result<()> {
    // A round with no share rows would satisfy the check below trivially. That
    // is the vacuous pass this whole suite exists to avoid.
    anyhow::ensure!(
        !snapshot.deliveries.is_empty(),
        "the round records no shares at all, so nothing about their completion can be true of it"
    );
    let unconfirmed: Vec<_> = snapshot
        .deliveries
        .iter()
        .filter(|delivery| !delivery.confirmed)
        .map(|delivery| delivery.key())
        .collect();
    anyhow::ensure!(
        unconfirmed.is_empty(),
        "{} share(s) ended the round unconfirmed, so the fleet's coming and going cost the \
         round work it owed: {unconfirmed:?}",
        unconfirmed.len()
    );
    Ok(())
}

/// How many helpers hold each share, least and most.
///
/// Reported rather than asserted, for the reason
/// [`assert_every_share_is_confirmed`] explains. Printed every run so a change
/// in the fleet's effective redundancy stays visible even though no rule names
/// it.
pub fn placement_spread(snapshot: &DurableSnapshot) -> (usize, usize) {
    let counts = snapshot
        .deliveries
        .iter()
        .map(|delivery| delivery.sent.len());
    let least = counts.clone().min().unwrap_or_default();
    let most = counts.max().unwrap_or_default();
    (least, most)
}

/// A resumed run must not re-POST to a helper that already accepted a share,
/// while any helper it has never tried with that share remains.
///
/// Stated with the condition because the unconditional claim is false, and
/// knowing why is the point. Recovery walks untried helpers first, then
/// interrupted ones, and only once a share is overdue does it extend to
/// ambiguous and then to previously accepted helpers — a deliberate,
/// duplicate-safe last resort. What must never happen is reaching for that last
/// resort while untried helpers are still available, because that spends a
/// re-send on a helper that already has the share and leaves the deficit
/// unfilled.
///
/// This is the assertion a one-URL suite could not even express: with a single
/// configured helper, "the helper that accepted" and "the only helper there is"
/// are the same server.
///
/// # Why the evidence has to be per share
///
/// `contacted` is keyed by the share, not a flat set of URLs, and it must stay
/// that way. A run legitimately POSTs many different shares to the same helper,
/// so a fleet-wide set of "helpers this run contacted" would flag helper *H* as
/// a premature re-send for share *A* merely because the run correctly delivered
/// share *B* to it. Only the SDK's own tracking report carries the share
/// identity alongside the helper; the route's contact log does not, which is
/// why that log cannot answer this question and is not used here.
pub fn assert_no_premature_resend_to_an_accepted_helper(
    before: &DurableSnapshot,
    configured: &[String],
    contacted: &BTreeMap<(i64, i64, i64), BTreeSet<String>>,
) -> Result<()> {
    let fleet: BTreeSet<String> = configured.iter().cloned().collect();
    for delivery in &before.deliveries {
        let Some(reached) = contacted.get(&delivery.key()) else {
            continue;
        };
        let untried: BTreeSet<String> = fleet.difference(&delivery.touched()).cloned().collect();
        if untried.is_empty() {
            // Every helper has been tried with this share, so a re-send to one
            // that accepted is the legal last resort rather than a wasted
            // attempt.
            continue;
        }
        let resent: Vec<_> = delivery.sent.intersection(reached).cloned().collect();
        anyhow::ensure!(
            resent.is_empty(),
            "D5 VIOLATED: share {:?} was re-POSTed to {resent:?}, which had already accepted \
             it, while {} helper(s) had never been tried with it at all: {untried:?}",
            delivery.key(),
            untried.len()
        );
    }
    Ok(())
}

/// A share must never be POSTed to a helper outside the configured fleet.
///
/// The confidentiality claim rests on it. A share delivered to an endpoint the
/// host never configured is a share disclosed to a party the voter never chose,
/// and no durable row would show it — the journal records where the wallet
/// *believes* it sent, so a delivery to an unconfigured host is exactly the
/// kind of thing only a contact record can catch.
pub fn assert_no_contact_outside_the_fleet(
    configured: &[String],
    contacted: &BTreeSet<String>,
) -> Result<()> {
    let fleet: BTreeSet<&str> = configured.iter().map(String::as_str).collect();
    let strangers: Vec<_> = contacted
        .iter()
        .filter(|url| !fleet.contains(url.as_str()))
        .cloned()
        .collect();
    anyhow::ensure!(
        strangers.is_empty(),
        "a share was POSTed to {strangers:?}, which the run never configured as helpers"
    );
    Ok(())
}

/// Every helper whose answer the wallet could not learn must stay journaled.
///
/// `D1` generalized from "some helper" to "every helper of the kind the rule is
/// about". The reservation is what makes an interrupted attempt recoverable
/// rather than invisible, so a helper left holding a share with no durable
/// record is a share the wallet could lose track of entirely — and with one
/// configured helper the general and particular statements were the same.
///
/// # Why refusals are excluded, having once been included
///
/// The first live run of this assertion reported six violations, and they were
/// this assertion's fault rather than the SDK's. A refused connection is a
/// *definite pre-dispatch* failure: the wallet knows no request byte left, so
/// it clears the reservation, and that is correct — there is nothing to
/// recover, and keeping the row would make the wallet poll a helper that
/// provably never received the share.
///
/// The rule only ever applied to attempts whose outcome is **unknowable**. A
/// helper that accepted the connection and never answered is the case that
/// matters: from the sidecar it is indistinguishable from a helper that
/// processed the share, so the record has to survive.
///
/// Checked at fleet level rather than per share, because the contact record
/// names the helper but not which share was in flight, and inventing that link
/// would assert more than the evidence supports.
pub fn assert_every_unanswered_helper_was_journalled(
    snapshot: &DurableSnapshot,
    unanswered: &BTreeSet<String>,
) -> Result<()> {
    let journalled: BTreeSet<String> = snapshot
        .deliveries
        .iter()
        .flat_map(|delivery| delivery.touched())
        .collect();
    let forgotten: Vec<_> = unanswered.difference(&journalled).cloned().collect();
    anyhow::ensure!(
        forgotten.is_empty(),
        "D1 VIOLATED: {forgotten:?} accepted a share POST and never answered it, yet the \
         round holds no durable record of the attempt. From the sidecar that is \
         indistinguishable from a helper never contacted, so nothing would ever reconcile it"
    );
    Ok(())
}

/// Helper placement must never exceed the fleet a run was given.
///
/// A cheap consistency check on the snapshot itself, so a scenario that
/// mis-wires its fleet fails as a configuration error rather than as a
/// mysterious placement result forty minutes later.
pub fn assert_placement_stays_within_the_fleet(
    snapshot: &DurableSnapshot,
    configured: &[String],
) -> Result<()> {
    let fleet: BTreeSet<&str> = configured.iter().map(String::as_str).collect();
    for delivery in &snapshot.deliveries {
        let outside: Vec<_> = delivery
            .touched()
            .into_iter()
            .filter(|url| !fleet.contains(url.as_str()))
            .collect();
        anyhow::ensure!(
            outside.is_empty(),
            "share {:?} is journaled against {outside:?}, which is not in the configured fleet",
            delivery.key()
        );
    }
    Ok(())
}

/// A run armed to hang must actually have hung, on the class it named.
///
/// The same rule the crash matrix applies to a stage that stops firing, and for
/// the same reason. A run whose stall never fired would satisfy every
/// assertion about "the state a hang left" while proving nothing, because the
/// state inspected would simply be a healthy round.
pub fn assert_the_stall_fired(
    records: &[crate::stall::StallRecord],
    target: crate::stall::StallTarget,
    point: crate::stall::StallPoint,
) -> Result<()> {
    let matching: Vec<_> = records.iter().filter(|record| record.is(target)).collect();
    anyhow::ensure!(
        !matching.is_empty(),
        "{target} never stopped answering; the run made no request of that class, so its \
         crash seam has stopped firing rather than the SDK having handled a hang. Recorded \
         instead: {:?}",
        records
            .iter()
            .map(|record| &record.target)
            .collect::<Vec<_>>()
    );
    let after_dispatch = point == crate::stall::StallPoint::AfterDispatch;
    anyhow::ensure!(
        matching
            .iter()
            .all(|record| record.after_dispatch == after_dispatch),
        "{target} hung at the wrong point: asked for {point:?}, and the dispatch hook \
         {} fired",
        if after_dispatch {
            "never"
        } else {
            "nonetheless"
        }
    );
    Ok(())
}

/// A hung request must be ended by the SDK, not by the suite's patience.
///
/// The claim this whole axis exists for, and the only one a crash exercise
/// cannot make. Nothing in this repository proved, before it, that *any*
/// request the wallet makes carries a deadline it actually keeps — and the PIR
/// path is the one where a host supplying its own transport has no SDK-side
/// bound at all.
///
/// `allowance` is a generous multiple of the class's declared bound rather than
/// the bound itself: a run makes many requests, several of them retried, and
/// the claim is that the hang ends, not that it ends promptly.
pub fn assert_the_request_was_bounded(
    target: crate::stall::StallTarget,
    elapsed: std::time::Duration,
    allowance: std::time::Duration,
) -> Result<()> {
    anyhow::ensure!(
        elapsed <= allowance,
        "{target} hung for {elapsed:?} without the SDK ending it, against a declared bound of \
         {:?} and an allowance of {allowance:?}. A request with no effective deadline wedges \
         the round it belongs to, and no restart repairs it because nothing crashed",
        target.declared_bound()
    );
    Ok(())
}

/// A hang that carried a transaction must leave its submission recoverable.
///
/// The conservative half, and deliberately only the half the evidence supports.
/// A stalled POST that may have been delivered must leave the row that says a
/// transaction might exist: a restarted process cannot prove the bytes never
/// left, so a row that vanished would let the next pass reserve a fresh first
/// attempt and build a second transaction spending the same notes. That is the
/// same claim `B1` makes about a crash before broadcast.
///
/// What is deliberately *not* asserted is the exact state the row settles in.
/// Normalization is lazy — it happens inside the lifecycle's next admission
/// rather than at open — so reading a particular name here would be asserting
/// the SDK's scheduling rather than its safety.
pub fn assert_a_stalled_submission_survived(
    snapshot: &DurableSnapshot,
    target: crate::stall::StallTarget,
    point: crate::stall::StallPoint,
) -> Result<()> {
    if !target.carries_a_submission() || point != crate::stall::StallPoint::AfterDispatch {
        return Ok(());
    }
    let kind = match target {
        crate::stall::StallTarget::DelegationPost => "delegation",
        _ => "vote",
    };
    anyhow::ensure!(
        snapshot
            .submissions
            .iter()
            .any(|submission| submission.kind == kind),
        "a {kind} POST hung after its bytes may have left and no submission row survived; a \
         restarted process cannot prove the request was never delivered, so the row must \
         outlive the hang rather than disappear with it"
    );
    Ok(())
}
