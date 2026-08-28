#[cfg(all(feature = "upstream", feature = "zakura"))]
compile_error!("features `upstream` and `zakura` cannot be enabled together");

#[cfg(not(any(feature = "upstream", feature = "zakura")))]
compile_error!("enable exactly one of the `upstream` or `zakura` features");

// `zcash_voting` and `zcash_voting-zakura` (extern name
// `zcash_voting_zakura`) are backend-specific facades over one implementation
// crate. Under the `zakura` feature, the upstream facade is inactive, so this
// alias binds the name the rest of this crate uses (`zcash_voting::...`) to
// the Zakura facade.
#[cfg(feature = "zakura")]
extern crate zcash_voting_zakura as zcash_voting;

pub mod example_capability_handoff;
pub mod example_config;
pub mod example_delegation;
pub mod example_recovery;
pub mod example_vote;
pub mod example_wire;
