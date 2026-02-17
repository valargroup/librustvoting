//! Standalone ceremony bootstrap test.
//!
//! Runs the EA key ceremony (register Pallas key → deal EA key → auto-ack)
//! so the chain reaches CONFIRMED and is ready for voting session creation.
//!
//! Usage (chain must be running via `make init && make start`):
//!
//!   cargo test --release --manifest-path e2e-tests/Cargo.toml \
//!     ceremony_bootstrap -- --nocapture --ignored

#[test]
#[ignore = "requires running chain"]
fn ceremony_bootstrap() {
    let (ea_sk, ea_pk) = e2e_tests::setup::load_ea_keypair();
    e2e_tests::setup::bootstrap_ceremony(&ea_sk, &ea_pk);
}
