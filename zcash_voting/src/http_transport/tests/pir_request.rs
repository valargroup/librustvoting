//! PIR request classification performed by the transport itself.

use super::super::{HyperTransport, PirHttpFailure, PirHttpFailurePhase};

#[tokio::test]
async fn an_unbuildable_pir_url_is_a_build_failure_not_an_outage() {
    let transport = HyperTransport::new();

    let error = match pir_client::Transport::get(&transport, "http://pir server.invalid/root").await
    {
        Ok(_) => panic!("a URL with a space cannot be built"),
        Err(error) => error,
    };

    let typed = PirHttpFailure::from_error_chain(&error).expect("a typed failure is attached");
    assert_eq!(typed.phase, PirHttpFailurePhase::Build);
    assert_eq!(typed.http_status, None);
    assert!(!typed.retryable());
    assert!(
        error.to_string().contains("build PIR request URL"),
        "{error:#}"
    );
}
