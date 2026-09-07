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

use std::collections::BTreeMap;

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
    pub pczt_persisted: bool,
    /// Whether a cached vote-commitment tree exists.
    pub cached_tree: bool,
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

/// The comparable shape of a finished round. See [`DurableSnapshot::shape`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RoundShape {
    submissions: BTreeMap<(String, i64, Option<i64>), String>,
    bundles: i64,
    proofs: i64,
    votes: i64,
    helper_share_plans: i64,
    share_delegations: i64,
    accepted_urls: usize,
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
            pczt_persisted: connection
                .query_row(
                    "select count(*) from bundles where pczt_sighash is not null",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0,
            cached_tree: count("cached_tree_state") > 0,
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
            accepted_urls: self.accepted_urls,
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
            require_step(
                plan,
                &NextStep::Delegate {
                    bundle_index: bundle,
                },
                stage,
            )?;
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
                    .any(|submission| submission.kind == "delegation" && submission.state == "submitting"),
                "B1 VIOLATED: the abandoned reservation is gone after reopen (states {:?});                  a restarted process cannot prove the bytes never left, so the row must                  survive as in-flight work rather than be discarded",
                snapshot.states()
            );
            require_step(
                plan,
                &NextStep::AdvanceDelegation {
                    bundle_index: bundle,
                },
                stage,
            )?;
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
            require_step(
                plan,
                &NextStep::AdvanceDelegation {
                    bundle_index: bundle,
                },
                stage,
            )?;
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
    let vote_submission = snapshot
        .submissions
        .iter()
        .any(|submission| submission.kind == "vote" || submission.kind == "vote_batch");
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
             where kind = 'delegation' and confirmation_source is not null limit 1",
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
             where kind = 'delegation' and confirmed_transaction_hash is not null limit 1",
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
