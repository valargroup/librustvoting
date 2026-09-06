//! The child process: drive a round, and die where told.
fn main() {
    eprintln!("worker: not yet wired to a provisioned round");
    std::process::exit(recovery_conformance::child::EXIT_STAGE_NEVER_REACHED);
}
