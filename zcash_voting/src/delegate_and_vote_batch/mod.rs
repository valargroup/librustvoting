//! Atomic delegation followed by an ordered batch of votes.
//!
//! The Rust API calls this `delegate_and_vote_batch`. The vote-sdk endpoint,
//! event name, signing domain, and existing recovery encodings retain their
//! protocol names so this naming change preserves transaction compatibility.
//!
//! The delegation-created VAN is ephemeral: it is the first proof's synthetic
//! anchor, never a leaf whose position a wallet can recover from the chain.

mod authorization;
mod preparation;
mod wire;

pub(crate) use preparation::DelegationAuthorization;
pub use preparation::{
    persist_delegate_and_vote_batch, prepare_delegate_and_vote_batch,
    recover_delegate_and_vote_batch, DelegateAndVoteBatchRequest,
};

pub(crate) use authorization::delegate_and_vote_batch_sighash;

#[cfg(test)]
mod tests;
