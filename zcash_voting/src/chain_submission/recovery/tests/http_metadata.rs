use super::*;

#[tokio::test]
async fn json_media_type_accepts_case_variants_and_delimiter_whitespace() {
    let responses = tree_responses(&[[3; 32], [4; 32]])
        .into_iter()
        .map(|response| {
            ChainHttpResponse::new(
                response.status(),
                response.body().to_vec(),
                Some("Application/JSON ; charset=utf-8".to_string()),
                response.headers().to_vec(),
            )
        })
        .collect();

    let (outcome, _) = scan_responses(responses, None).await.unwrap();

    assert!(matches!(outcome, RecoveryScanOutcome::Match { .. }));
}
