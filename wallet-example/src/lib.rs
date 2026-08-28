#[cfg(all(feature = "lrz", feature = "zakura"))]
compile_error!("features `lrz` and `zakura` cannot be enabled together");

#[cfg(not(any(feature = "lrz", feature = "zakura")))]
compile_error!("enable exactly one of the `lrz` or `zakura` features");

// The LRZ facade is bound to the name the rest of this crate uses. Under the
// `zakura` feature, the direct `zcash_voting` dependency already has that
// extern name and selects the implementation's Zakura backend.
#[cfg(feature = "lrz")]
extern crate zcash_voting_lrz as zcash_voting;

pub mod example_capability_handoff;
pub mod example_config;
pub mod example_delegation;
pub mod example_recovery;
pub mod example_vote;
pub mod example_wire;
