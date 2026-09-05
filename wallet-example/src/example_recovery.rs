use std::sync::Arc;

use zcash_voting::prelude::{
    ChainSubmissionClientConfig, ChainSubmissionControl, HelperClient, HelperHealth, Network,
    NoopRoundDriveReporter, ProposalRosterEntry, RoundBinding, RoundDrivePolicy, RoundDriver,
    RoundExecutor, RoundHostContext, RoundHostSourceBridge, RoundRunReport, VotingDb,
};
use zcash_voting::wire::PirLayout;
use zcash_voting::{
    ChainSubmissionFailure, HelperTransport, HyperTransport, PirFleet, RouteHttp, VotingError,
};

/// Why [`advance_round_until_idle`] could not start a run.
///
/// A step that fails during the run is not an error here: the driver isolates
/// it and keeps it, with everything it had already done, in
/// [`RoundRunReport::failures`].
#[derive(Debug)]
pub enum RoundAdvanceError {
    /// The executor could not be built over the chain configuration.
    Executor(ChainSubmissionFailure),
    /// The round binding was refused.
    Binding(VotingError),
}

impl std::fmt::Display for RoundAdvanceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Executor(failure) => write!(f, "build round executor: {}", failure.message()),
            Self::Binding(error) => write!(f, "bind round executor: {error}"),
        }
    }
}

impl std::error::Error for RoundAdvanceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Executor(failure) => Some(failure),
            Self::Binding(error) => Some(error),
        }
    }
}

/// A PIR fleet whose requests travel `route`, for the `host` closure passed
/// to [`advance_round_until_idle`].
///
/// Delegation PIR runs over the fleet in `RoundHostContext::delegation`,
/// which the host builds, not over the executor's transports; a wallet that
/// requires a private route builds its fleet here with the same route it
/// passes to the loop, so no PIR request falls back to a direct connection.
pub fn routed_pir_fleet<R: RouteHttp>(
    route: Arc<R>,
    endpoints: &[String],
    layout: PirLayout,
) -> Result<PirFleet, VotingError> {
    PirFleet::new(
        endpoints,
        layout,
        Arc::new(HyperTransport::with_shared_route(route)),
    )
}

/// Drives one round to quiescence with the SDK-owned driver.
///
/// The driver owns the loop: it re-plans from durable state, runs the
/// obligations the plan lists, overlaps independent bundles up to `policy`,
/// paces a still-tracking submission, isolates failures per bundle, and stops
/// at the first state only a host can resolve. A wallet supplies transports,
/// the fleet, timing and cancellation, and reads
/// [`RoundRunReport::quiescence`] to learn what to do next — an open ballot,
/// delegation signatures it has not collected, a terminal submission, or
/// nothing left to do.
///
/// `route` carries every request the executor makes itself: helper POSTs,
/// vote-chain calls, and vote-tree sync all run through it, so a wallet that
/// requires Tor or another privacy route passes its executor once and none of
/// those fall back to a direct connection. Pass
/// `Arc::new(DirectRoute::default())` when no route is required. Delegation
/// PIR is the exception: it runs over the `PirFleet` inside the
/// `RoundHostContext` that `host` returns, which this helper never sees. Build
/// that fleet with [`routed_pir_fleet`] over the same `route`; a fleet built
/// over a direct transport sends PIR requests directly regardless of `route`.
///
/// `helper_health` is the wallet's helper score table. It is caller-owned so
/// that failures and cooldowns observed in one call still steer helper
/// selection in the next: a wallet that schedules this helper repeatedly keeps
/// one `HelperHealth` per wallet and passes a clone each time.
///
/// `host` is called once per dispatch, not once per run, so each step sees the
/// current time and fleet: a run can take minutes, and a long proof can cross
/// the last-moment or vote-end boundary.
pub async fn advance_round_until_idle<R: RouteHttp>(
    voting_db: Arc<VotingDb>,
    network: Network,
    chain_endpoints: Vec<String>,
    route: Arc<R>,
    helper_health: HelperHealth,
    binding: RoundBinding,
    host: impl Fn() -> RoundHostContext + Send + Sync,
    policy: RoundDrivePolicy,
    control: &ChainSubmissionControl,
) -> Result<RoundRunReport, RoundAdvanceError> {
    // One transport, and so one blocking runtime, serves helpers, the chain,
    // and the vote tree; each `HyperTransport` owns worker threads.
    let transport = Arc::new(HyperTransport::with_shared_route(route));
    let helper_transport: Arc<dyn HelperTransport> = transport.clone();
    let helper_client = HelperClient::new(helper_transport, helper_health);
    let executor = RoundExecutor::with_transport(
        voting_db,
        Arc::clone(&transport),
        ChainSubmissionClientConfig::for_network(network, chain_endpoints),
        helper_client,
    )
    .map_err(RoundAdvanceError::Executor)?
    .with_binding(binding)
    .map_err(RoundAdvanceError::Binding)?
    .with_tree_transport(transport);

    Ok(RoundDriver::new(&executor)
        .with_policy(policy)
        .run(
            &RoundHostSourceBridge::new(host),
            control,
            &NoopRoundDriveReporter {},
        )
        .await)
}

/// Builds the executor binding from the authenticated proposal roster.
pub fn round_binding(
    round_id: &str,
    network: Network,
    proposals: &[(u32, u32)],
    hotkey_secret: Option<Vec<u8>>,
) -> RoundBinding {
    RoundBinding {
        round_id: round_id.to_string(),
        network,
        proposals: proposals
            .iter()
            .map(|(proposal_id, num_options)| ProposalRosterEntry {
                proposal_id: *proposal_id,
                num_options: *num_options,
            })
            .collect(),
        hotkey_secret: hotkey_secret.map(zeroize::Zeroizing::new),
    }
}
