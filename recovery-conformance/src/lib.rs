//! Staging crash-recovery conformance for `zcash_voting`.
//!
//! The crate's own suite proves that durable rows are written in the right
//! order. It cannot prove the claim those rows exist to support: that an app
//! *killed* mid-round — no unwinding, no flush, no graceful close — restarts
//! against the same sidecar and the same live chain and converges, without
//! spending a note twice or losing a vote. Every existing test ends in a clean
//! `drop`, and a clean drop is the one thing a crash is not.
//!
//! This package closes that gap. It provisions a real multi-proposal,
//! multi-bundle round on staging, drives it in a child process, kills that
//! child at a named durable boundary, and then asks the reopened sidecar the
//! only question that matters: does the round still know what it owes?
//!
//! The oracle is `zcash_voting::session::resume_plan`, which is a pure
//! function of durable state. Nothing in memory survives the crash, so the
//! plan after reopen *is* the complete definition of the remaining work — and
//! every assertion here is ultimately about that value.
//!
//! Deliberately outside the workspace default members: it needs the network,
//! it kills processes, and it must never run as part of `make test`.

pub mod assertions;
pub mod child;
pub mod environment;
pub mod provisioning;
pub mod round_run;
pub mod run_config;
pub mod signing;
pub mod stage_config;
pub mod stages;
pub mod wallet_sync;

pub use stages::{BroadcastPoint, CrashStage, CrashTrigger, SubmissionKind};
