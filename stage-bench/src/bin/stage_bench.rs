//! The one command.
//!
//! `run` provisions a staging round over the ballot you describe, drives it,
//! and prints where the time went. `analyze` re-derives that report from a
//! finished run directory without touching the network.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use recovery_conformance::helper_fleet::{HelperFleetPlan, SYNTHETIC_HELPER_URLS};
use recovery_conformance::round_run::{endpoints_from, endpoints_with_fleet, helper_backend};
use stage_bench::ballot::{Ballot, DEFAULT_OPTION_WIDTHS};

/// The SDK's ceiling on concurrent batch proofs (`MAX_BATCH_PROOF_CONCURRENCY`).
///
/// Mirrored because the constant is private. Checked here so an out-of-range
/// request fails before a round is provisioned rather than inside proving.
const MAX_PROOF_CONCURRENCY: usize = 15;
use stage_bench::events::EventLog;
use stage_bench::metrics::{render, Metrics};
use stage_bench::preflight::{self, Preflight};
use stage_bench::run_config::{BenchOutcome, BenchRunConfig};
use stage_bench::{supervise, Manifest};

#[derive(Parser, Debug)]
#[command(
    name = "stage-bench",
    about = "Multi-proposal staging benchmark for the zcash_voting SDK",
    long_about = "Provisions one round on the staging vote chain with a ballot you choose, \
                  drives a complete vote, and reports per-phase and per-proposal timings with \
                  the concurrency each phase reached.\n\n\
                  Run it under Infisical so the voter and coordinator credentials reach it \
                  without touching disk:\n\n  \
                  infisical run --env=staging -- stage-bench run --proposals 37"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Provision a round, drive it, and report.
    Run(RunArgs),
    /// Re-derive the report from a finished run directory.
    Analyze {
        /// The run directory to read.
        run_dir: PathBuf,
    },
    /// Resolve everything a run needs and stop, without provisioning.
    ///
    /// Every check `run` performs before it spends a round, and nothing after.
    /// Use it to confirm credentials, the wallet, the published deployment, and
    /// the ballot on a machine that has not run the benchmark before.
    Preflight(PreflightArgs),
}

#[derive(Parser, Debug)]
struct PreflightArgs {
    /// Proposals the ballot would have.
    #[arg(long, default_value_t = 37)]
    proposals: usize,

    /// Option counts, cycled across the ballot.
    #[arg(long, value_delimiter = ',', default_values_t = DEFAULT_OPTION_WIDTHS.to_vec())]
    option_widths: Vec<usize>,

    /// A round export to validate instead of a synthetic ballot.
    #[arg(long, conflicts_with_all = ["proposals", "option_widths"])]
    ballot: Option<PathBuf>,
}

#[derive(Parser, Debug)]
struct RunArgs {
    /// Proposals on the ballot.
    #[arg(long, default_value_t = 37)]
    proposals: usize,

    /// Option counts, cycled across the ballot. Each must be 2 to 8.
    #[arg(long, value_delimiter = ',', default_values_t = DEFAULT_OPTION_WIDTHS.to_vec())]
    option_widths: Vec<usize>,

    /// A vote manager's round export to replay instead of a synthetic ballot.
    #[arg(long, conflicts_with_all = ["proposals", "option_widths"])]
    ballot: Option<PathBuf>,

    /// Helpers to configure. One uses the real staging primary; more build a
    /// synthetic fleet whose members all route to it.
    #[arg(long, default_value_t = 1)]
    helpers: usize,

    /// Bundles the driver advances at once. Defaults to the SDK's own three, so
    /// a plain run measures what a host gets. Lower it to 1 for a cold-PIR run:
    /// staging serves PIR from one synchronous endpoint.
    #[arg(long, default_value_t = 3)]
    bundle_concurrency: usize,

    /// Vote-commitment proofs built at once within a bundle. Defaults to the
    /// SDK's `DEFAULT_BATCH_PROOF_CONCURRENCY`; 15 is its ceiling. A wide ballot
    /// builds one proof per proposal per bundle, so this is the other axis a
    /// 37-proposal round is serialized on.
    #[arg(long, default_value_t = 3)]
    proof_concurrency: usize,

    /// Seconds between provisioning and the round's vote end. Share scheduling
    /// derives its windows from this, so hold it fixed across compared runs.
    #[arg(long, default_value_t = 6 * 60 * 60)]
    vote_window: u64,

    /// Seconds the worker may run before it is killed. Derived from the
    /// workload when omitted.
    #[arg(long)]
    budget: Option<u64>,

    /// Seconds the confirmation phase may run. Confirmation is background work
    /// a wallet spreads across the voting window, so this bounds the benchmark,
    /// not the round; an expiry is reported as an incomplete tail.
    #[arg(long, default_value_t = 30 * 60)]
    tracking_budget: u64,

    /// Focused confirmations to drive at once. 1 uses the shipped background
    /// tracker, whose pass polls shares one at a time. Above 1 replaces it with
    /// concurrent per-share confirmation — an experiment that measures what
    /// that serial walk costs, not shipped behaviour.
    #[arg(long, default_value_t = 1)]
    confirm_concurrency: usize,

    /// Detailed records retained per reported invocation. A capped capture
    /// cannot support a peak-concurrency claim, and says so.
    #[arg(long, default_value_t = 262_144)]
    max_records: usize,

    /// Skip the cached PIR proof template. Slower, and exposed to the one
    /// synchronous PIR endpoint.
    #[arg(long)]
    no_warm_pir: bool,

    /// Where run directories are created.
    #[arg(long, default_value = "runs")]
    out: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Commands::Run(args) => run(args).await,
        Commands::Analyze { run_dir } => analyze(&run_dir),
        Commands::Preflight(args) => preflight_only(args).await,
    }
}

/// Resolves and reports, provisioning nothing.
async fn preflight_only(args: PreflightArgs) -> Result<()> {
    let ballot = match &args.ballot {
        Some(path) => Ballot::from_export(
            &std::fs::read(path)
                .with_context(|| format!("reading the ballot at {}", path.display()))?,
        )?,
        None => Ballot::synthetic(args.proposals, &args.option_widths)?,
    };
    let resolved = preflight::resolve().await?;

    println!("ballot            {} proposals", ballot.len());
    println!(
        "  option widths   {:?}",
        ballot
            .proposals()
            .iter()
            .map(|proposal| proposal.options.len())
            .collect::<Vec<_>>()
    );
    println!("worker            {}", resolved.worker.display());
    println!("wallet            {}", resolved.wallet_db.display());
    println!("account           {}", resolved.account_uuid);
    println!(
        "warm PIR          {}",
        resolved
            .warm_pir
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "none (runs will fetch every proof live)".to_string())
    );
    println!("coordinator       {}", resolved.keyring.address());
    println!(
        "vote servers      {:?}",
        resolved.deployment.vote_server_urls()
    );
    println!("PIR endpoints     {:?}", resolved.deployment.pir_urls());
    println!("\nready: `stage-bench run` would provision a round on the staging vote chain.");
    Ok(())
}

async fn run(args: RunArgs) -> Result<()> {
    let ballot = resolve_ballot(&args)?;
    anyhow::ensure!(
        args.helpers >= 1 && args.helpers <= SYNTHETIC_HELPER_URLS.len(),
        "the synthetic fleet has {} helper names, so --helpers takes 1 to {}",
        SYNTHETIC_HELPER_URLS.len(),
        SYNTHETIC_HELPER_URLS.len()
    );
    anyhow::ensure!(
        args.bundle_concurrency >= 1,
        "--bundle-concurrency must be at least 1"
    );
    anyhow::ensure!(
        args.confirm_concurrency >= 1,
        "--confirm-concurrency must be at least 1"
    );
    anyhow::ensure!(
        (1..=MAX_PROOF_CONCURRENCY).contains(&args.proof_concurrency),
        "--proof-concurrency takes 1 to {MAX_PROOF_CONCURRENCY}, the SDK's own ceiling"
    );
    if args.confirm_concurrency > 1 {
        eprintln!(
            "bench: confirming {} shares at a time through focused confirmation. This \
             replaces the shipped tracker and measures what its serial walk costs; it is \
             not a measurement of shipped behaviour.",
            args.confirm_concurrency
        );
    }

    let started_at_unix = now_unix();
    let preflight = preflight::resolve().await?;
    if args.bundle_concurrency > 1 && (args.no_warm_pir || preflight.warm_pir.is_none()) {
        // Not refused, because a deliberate cold run at width is a legitimate
        // experiment about the PIR endpoint. Named, because the failure it
        // produces — an endpoint that stops answering — reads as a delivery
        // problem rather than as the load this flag applied.
        eprintln!(
            "bench: WARNING — {} bundles wide with no warm PIR cache. Staging serves PIR \
             from one synchronous endpoint and stops answering under concurrent queries. \
             Use --bundle-concurrency 1 for a cold run.",
            args.bundle_concurrency
        );
    }
    eprintln!(
        "bench: ballot of {} proposals, {} helpers, bundle concurrency {}",
        ballot.len(),
        args.helpers,
        args.bundle_concurrency
    );

    eprintln!("bench: provisioning a round on staging");
    let round = stage_bench::provision::provision(&preflight, &ballot, args.vote_window).await?;
    eprintln!("bench: round {}", round.round_id);

    let run_dir = args.out.join(format!(
        "{}-{}",
        timestamp(),
        &round.round_id[..round.round_id.len().min(12)]
    ));
    std::fs::create_dir_all(&run_dir).context("creating the run directory")?;

    let config = build_config(&args, &preflight, &ballot, &round, &run_dir)?;
    let config_path = BenchRunConfig::path_in(&run_dir);
    config.write(&config_path)?;

    let budget = Duration::from_secs(args.budget.unwrap_or_else(|| default_budget(&config)));
    eprintln!("bench: driving the round (budget {budget:?})");
    let child = supervise::run(&preflight.worker, &config_path, budget)?;

    // The report is built before the child's status is judged. A run that
    // failed part way is exactly the run whose phase table is worth reading,
    // and refusing to print it would discard the evidence.
    report(&run_dir, &config, started_at_unix, args.vote_window)?;
    refresh_warm_pir(&preflight, &config);
    supervise::require_completion(child, budget)
}

fn analyze(run_dir: &std::path::Path) -> Result<()> {
    let manifest = Manifest::read(run_dir).context("reading the run manifest")?;
    let snapshots = stage_bench::read_snapshots(run_dir)?;
    let events = EventLog::read(run_dir).unwrap_or_default();
    let metrics = Metrics::derive(&snapshots, &events);
    write_metrics(run_dir, &metrics)?;
    print!("{}", render(&manifest, &metrics));
    Ok(())
}

/// Builds the manifest and metrics for a finished run and prints the table.
fn report(
    run_dir: &std::path::Path,
    config: &BenchRunConfig,
    started_at_unix: u64,
    vote_window_seconds: u64,
) -> Result<()> {
    let outcome = BenchOutcome::read(&BenchOutcome::path_in(run_dir)).unwrap_or_else(|_| {
        // The child died before writing one. Everything it did write still
        // describes a real partial run, so the report is built over a placeholder
        // rather than abandoned.
        eprintln!("bench: the worker wrote no outcome; reporting what it captured");
        BenchOutcome {
            quiescence: "NoOutcomeWritten".to_string(),
            quiescence_kind: "NoOutcomeWritten".to_string(),
            failures: Vec::new(),
            notes: 0,
            bundles: 0,
            proposals: config.ballot.len(),
            completed_proposals: 0,
            tracking: Vec::new(),
            round_drive_seconds: 0.0,
            tracking_seconds: 0.0,
        }
    });

    let manifest = Manifest::build(config, &outcome, started_at_unix, vote_window_seconds);
    manifest.write(run_dir)?;

    let snapshots = stage_bench::read_snapshots(run_dir)?;
    let events = EventLog::read(run_dir).unwrap_or_default();
    let metrics = Metrics::derive(&snapshots, &events);
    write_metrics(run_dir, &metrics)?;
    print!("{}", render(&manifest, &metrics));

    if !outcome.is_complete() {
        eprintln!(
            "bench: the round did not finish cleanly ({}); the timings above describe a partial \
             run",
            outcome.quiescence_kind
        );
        for failure in &outcome.failures {
            eprintln!("bench: failure {}: {}", failure.kind, failure.message);
        }
    }
    Ok(())
}

fn write_metrics(run_dir: &std::path::Path, metrics: &Metrics) -> Result<()> {
    std::fs::write(
        run_dir.join("metrics.json"),
        serde_json::to_vec_pretty(metrics)?,
    )
    .context("writing metrics.json")
}

fn resolve_ballot(args: &RunArgs) -> Result<Ballot> {
    match &args.ballot {
        Some(path) => {
            let raw = std::fs::read(path)
                .with_context(|| format!("reading the ballot at {}", path.display()))?;
            Ballot::from_export(&raw)
        }
        None => Ballot::synthetic(args.proposals, &args.option_widths),
    }
}

fn build_config(
    args: &RunArgs,
    preflight: &Preflight,
    ballot: &Ballot,
    round: &stage_bench::provision::ProvisionedRound,
    run_dir: &std::path::Path,
) -> Result<BenchRunConfig> {
    // One helper is the real primary; more are synthetic names the route
    // rewrites onto it, which is the only way to exercise fan-out against a
    // staging deployment where a single host answers the share endpoint.
    let fleet = if args.helpers > 1 {
        HelperFleetPlan::all_answering(helper_backend(&preflight.deployment), args.helpers)
    } else {
        HelperFleetPlan::none()
    };
    let endpoints = if args.helpers > 1 {
        endpoints_with_fleet(&preflight.deployment, &fleet)
    } else {
        endpoints_from(&preflight.deployment)
    };

    Ok(BenchRunConfig {
        sidecar: run_dir.join("sidecar.db"),
        wallet_db: preflight.wallet_db.clone(),
        warm_pir_from: (!args.no_warm_pir)
            .then(|| preflight.warm_pir.clone())
            .flatten(),
        round_id: round.round_id.clone(),
        account_uuid: preflight.account_uuid.clone(),
        endpoints,
        ballot: ballot.clone(),
        fleet,
        vote_end_time_seconds: round.vote_end_time_seconds,
        bundle_concurrency: args.bundle_concurrency,
        proof_concurrency: args.proof_concurrency,
        tracking_budget_seconds: args.tracking_budget,
        confirm_concurrency: args.confirm_concurrency,
        max_dispatches: max_dispatches(ballot.len()),
        max_records: args.max_records,
        run_dir: run_dir.to_path_buf(),
    })
}

fn refresh_warm_pir(preflight: &Preflight, config: &BenchRunConfig) {
    let Some(template) = &preflight.warm_pir else {
        return;
    };
    match preflight::refresh_warm_pir(template, &config.sidecar) {
        Ok(added) if added > 0 => eprintln!("bench: warmed {added} more PIR proofs"),
        Ok(_) => {}
        Err(error) => eprintln!("bench: could not refresh the PIR template: {error}"),
    }
}

/// Shares every vote commits, which sets the size of the delivery workload.
const SHARES_PER_VOTE: usize = 16;

/// Bundles the fixed voter wallet packs its notes into.
///
/// Used only to size the dispatch ceiling and the default budget. The run
/// reports the layout it actually got; being wrong here costs headroom, not
/// correctness.
const ASSUMED_BUNDLES: usize = 3;

/// Driver dispatches allowed before the run ends rather than looping.
///
/// Sized from the work a round can owe — a cast, an advance, a submission, and
/// a confirmation for every share of every proposal in every bundle — with a
/// generous multiplier. A ceiling that is too low ends a healthy 37-proposal
/// round as `PassBudgetExhausted`, which reads like a defect.
fn max_dispatches(proposals: usize) -> usize {
    (proposals * ASSUMED_BUNDLES * SHARES_PER_VOTE * 4).max(4_096)
}

/// How long the worker may run when the caller names no budget.
///
/// Scaled by the ballot, because proving dominates: a 37-proposal round builds
/// a vote commitment for every proposal in every bundle, and each is seconds of
/// release-build Halo2 work. Deliberately generous — this is a safety net for a
/// wedged run, not a target. A budget that expires on a healthy round throws
/// away the round *and* the measurement, while one that is too long only delays
/// the report of a run that was already lost.
fn default_budget(config: &BenchRunConfig) -> u64 {
    const PER_PROPOSAL_SECONDS: u64 = 40;
    const FLOOR_SECONDS: u64 = 60 * 60;
    const BUDGET_HEADROOM_SECONDS: u64 = 120;
    let proposals = config.ballot.len() as u64;
    let bundles = ASSUMED_BUNDLES as u64;
    // The confirmation budget is spent inside the same child, so the worker's
    // bound has to cover it as well as the drive, plus room to write the
    // outcome. A worker killed one second before it reports has measured
    // everything and delivered nothing.
    (proposals * bundles * PER_PROPOSAL_SECONDS).max(FLOOR_SECONDS)
        + config.tracking_budget_seconds
        + BUDGET_HEADROOM_SECONDS
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

/// A sortable UTC stamp for the run directory name.
///
/// Formatted by hand from the epoch rather than by pulling in a date library
/// for one string. Directory names sort chronologically, which is the only
/// property needed.
fn timestamp() -> String {
    let seconds = now_unix();
    let days = seconds / 86_400;
    let time = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// Howard Hinnant's `civil_from_days`, for a directory name.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    } as u32;
    (year + i64::from(month <= 2), month, day)
}
