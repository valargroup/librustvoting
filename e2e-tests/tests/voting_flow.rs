//! Happy-path E2E voting flow: create session, delegate (real ZKP #1), cast,
//! reveal, wait for TALLYING, second reveal, submit tally, finalize. No fixture
//! files — ElGamal and delegation proof generated inline.

use base64::Engine;
use e2e_tests::{
    api::{
        self, commitment_tree_next_index, get_json, post_json, post_json_accept_committed,
        tally_has_proposal, wait_for_round_status, SESSION_STATUS_FINALIZED, SESSION_STATUS_TALLYING,
    },
    elgamal::{self, homomorphic_add},
    payloads::{
        create_voting_session_payload, delegate_vote_payload, cast_vote_payload,
        reveal_share_payload, submit_tally_payload, ciphertext_to_base64, encrypt_share,
        TallyEntry,
    },
    setup::build_delegation_bundle_for_test,
};
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

const BLOCK_WAIT_MS: u64 = 6000;
const SESSION_CREATOR: &str = "zvote1admin";

fn log_step(step: &str, msg: &str) {
    eprintln!("[E2E] {}: {}", step, msg);
}

fn block_wait() {
    std::thread::sleep(std::time::Duration::from_millis(BLOCK_WAIT_MS));
}

fn round_id_hex(round_id: &[u8]) -> String {
    hex::encode(round_id)
}

/// Run vote-tree-cli with args. Returns stdout or panics on failure.
fn run_tree_cli(args: &[&str]) -> String {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../vote-commitment-tree-client/Cargo.toml");
    let out = std::process::Command::new("cargo")
        .args([
            "run",
            "--quiet",
            "--manifest-path",
            manifest.to_str().unwrap(),
            "--bin",
            "vote-tree-cli",
            "--",
        ])
        .args(args)
        .output()
        .expect("vote-tree-cli");
    if !out.status.success() {
        panic!(
            "vote-tree-cli failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
#[ignore = "requires running chain: make init && make start"]
fn voting_flow_full_lifecycle() {
    // ----- Setup: ElGamal keypair + delegation bundle (once, ~30-60s for proof) -----
    log_step("Setup", "generating ElGamal keypair and delegation bundle (K=14 proof may take 30-60s)...");
    let mut rng = ChaCha20Rng::seed_from_u64(42);
    let (_elgamal_sk, elgamal_pk) = elgamal::keygen(&mut rng);
    let ea_pk_bytes = elgamal::marshal_public_key(&elgamal_pk);

    let (delegation_bundle, session_fields) =
        build_delegation_bundle_for_test().expect("build_delegation_bundle_for_test");
    log_step("Setup", "delegation bundle ready");

    let (body, _fields, round_id) =
        create_voting_session_payload(&ea_pk_bytes, 120, Some(session_fields));
    let round_id_hex = round_id_hex(&round_id);

    // ----- Step 1: Create voting session -----
    log_step("Step 1", "create voting session");
    let (status, json) = post_json("/zally/v1/create-voting-session", &body)
        .expect("POST create-voting-session");
    assert_eq!(status, 200, "create session: expected HTTP 200, got {} body={:?}", status, json);
    let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    assert_eq!(code, 0, "create session rejected: {:?}", json.get("log"));
    block_wait();

    // ----- Step 2: Delegate vote (real ZKP #1) -----
    log_step("Step 2", "delegate vote (ZKP #1)");
    let deleg_body = delegate_vote_payload(&round_id, &delegation_bundle);
    let (status, json) = post_json_accept_committed(
        "/zally/v1/delegate-vote",
        &deleg_body,
        || commitment_tree_next_index().map(|n| n >= 2).unwrap_or(false),
    )
    .expect("POST delegate-vote");
    assert_eq!(status, 200, "delegate-vote: expected HTTP 200, got {} body={:?}", status, json);
    assert_eq!(
        json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1),
        0,
        "delegation rejected: {:?}",
        json.get("log")
    );
    block_wait();

    // ----- Step 3: Commitment tree has root after delegation -----
    log_step("Step 3", "commitment tree has root after delegation");
    let mut anchor_height: u32 = 0;
    for _ in 0..10 {
        let (status, json) = get_json("/zally/v1/commitment-tree/latest").expect("GET tree latest");
        assert_eq!(status, 200, "GET commitment-tree/latest: expected 200, got {} body={:?}", status, json);
        if let Some(tree) = json.get("tree") {
            let h = tree.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            if h > 0 {
                anchor_height = h;
                assert!(tree.get("root").is_some());
                assert!(tree.get("next_index").and_then(|x| x.as_u64()).unwrap_or(0) >= 2);
                break;
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
    assert!(anchor_height > 0, "tree never populated after delegation");

    // Step 2b: tree at anchor height
    let (status, json) = get_json(&format!("/zally/v1/commitment-tree/{}", anchor_height))
        .expect("GET tree at height");
    assert_eq!(status, 200, "GET tree at height: status={} body={:?}", status, json);
    assert!(json.get("tree").is_some());

    // Step 2c: Rust tree client sync (2 leaves)
    log_step("Step 2c", "tree CLI sync (2 leaves)");
    let base_url = api::base_url();
    let out = run_tree_cli(&["sync", "--node", &base_url]);
    assert!(out.contains("leaves synced:     2") || out.contains("leaves synced: 2"));
    assert!(out.contains("root match:") || out.contains("OK"));

    // Step 2d: witness position 0
    let out = run_tree_cli(&["witness", "--node", &base_url, "--position", "0"]);
    assert!(out.contains("Witness") && out.contains("bytes"));

    // ----- Step 4: Cast vote -----
    log_step("Step 4", "cast vote");
    let cast_body = cast_vote_payload(&round_id, anchor_height);
    let (status, json) = {
        let mut last = None;
        for attempt in 1..=3 {
            let result = post_json_accept_committed(
                "/zally/v1/cast-vote",
                &cast_body,
                || commitment_tree_next_index().map(|n| n >= 4).unwrap_or(false),
            )
            .expect("POST cast-vote");
            let code = result.1.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            if result.0 == 200 && code == 0 {
                last = Some(result);
                break;
            }
            eprintln!(
                "[E2E] Step 4 attempt {}: status={} code={} log={:?}",
                attempt,
                result.0,
                code,
                result.1.get("log").or(result.1.get("error"))
            );
            last = Some(result);
            if attempt < 3 {
                block_wait();
            }
        }
        last.expect("cast-vote: 3 attempts")
    };
    assert_eq!(status, 200, "cast-vote: expected 200, got {} body={:?}", status, json);
    let code = json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
    assert_eq!(
        code,
        0,
        "cast-vote rejected: code={} log={:?}",
        code,
        json.get("log").or(json.get("error"))
    );
    block_wait();

    // ----- Step 5: Tree updated after cast (4 leaves) -----
    log_step("Step 5", "tree updated after cast (4 leaves)");
    let (status, json) = get_json("/zally/v1/commitment-tree/latest").expect("GET tree latest");
    assert_eq!(status, 200, "GET tree latest: status={} body={:?}", status, json);
    let tree = json.get("tree").expect("tree");
    anchor_height = tree.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    assert_eq!(
        tree.get("next_index").and_then(|x| x.as_u64()).unwrap_or(0),
        4
    );

    // Step 4c: tree client sync 4 leaves
    let out = run_tree_cli(&["sync", "--node", &base_url]);
    assert!(out.contains("4"));

    // Step 4d: witness position 2
    let out = run_tree_cli(&["witness", "--node", &base_url, "--position", "2"]);
    assert!(out.contains("Witness"));

    // ----- Step 6: Reveal share (first) -----
    log_step("Step 6", "reveal share (first)");
    let enc_share_0 = encrypt_share(&elgamal_pk, 1, &mut rng);
    let reveal_body = reveal_share_payload(&round_id, anchor_height, &enc_share_0, 0, 1);
    let (status, json) = post_json_accept_committed(
        "/zally/v1/reveal-share",
        &reveal_body,
        || tally_has_proposal(&round_id_hex, 0),
    )
    .expect("POST reveal-share");
    assert_eq!(status, 200, "reveal-share: expected 200, got {} body={:?}", status, json);
    assert_eq!(json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1), 0);
    block_wait();

    // ----- Step 7: Tally has encrypted ciphertext -----
    log_step("Step 7", "tally has encrypted ciphertext");
    let (status, json) = get_json(&format!("/zally/v1/tally/{}/0", round_id_hex))
        .expect("GET tally");
    assert_eq!(status, 200, "GET tally: expected 200, got {} body={:?}", status, json);
    let tally = json.get("tally").expect("tally");
    assert!(tally.get("1").is_some());

    // ----- Step 8: Wait for TALLYING -----
    log_step("Step 8", "waiting for TALLYING (up to 250s)");
    wait_for_round_status(&round_id_hex, SESSION_STATUS_TALLYING, 250_000, 3_000)
        .expect("wait for TALLYING");
    let (_, json) = get_json(&format!("/zally/v1/round/{}", round_id_hex)).expect("GET round");
    assert_eq!(
        json.get("round").and_then(|r| r.get("status")).and_then(|s| s.as_i64()).unwrap(),
        SESSION_STATUS_TALLYING
    );

    // ----- Step 9: Reveal second share during TALLYING -----
    log_step("Step 9", "reveal second share during TALLYING");
    let enc_share_1 = encrypt_share(&elgamal_pk, 1, &mut rng);
    let reveal_body_1 = reveal_share_payload(&round_id, anchor_height, &enc_share_1, 0, 1);
    // Committed = tally equals HomomorphicAdd(share0, share1) so we don't treat 502 as success when only first reveal was in.
    let expected_accumulated_b64 = {
        let dec0 = base64::engine::general_purpose::STANDARD.decode(&enc_share_0).expect("decode enc_share_0");
        let dec1 = base64::engine::general_purpose::STANDARD.decode(&enc_share_1).expect("decode enc_share_1");
        let ct0 = elgamal::unmarshal(&dec0).expect("unmarshal ct0");
        let ct1 = elgamal::unmarshal(&dec1).expect("unmarshal ct1");
        ciphertext_to_base64(&homomorphic_add(&ct0, &ct1))
    };
    let (status, json) = post_json_accept_committed(
        "/zally/v1/reveal-share",
        &reveal_body_1,
        || {
            let (status, json) = match get_json(&format!("/zally/v1/tally/{}/0", round_id_hex)) {
                Ok(x) => x,
                Err(_) => return false,
            };
            let on_chain = (status == 200)
                .then(|| json.get("tally").and_then(|t| t.get("1")).and_then(|v| v.as_str()));
            on_chain
                .flatten()
                .map(|s| s == expected_accumulated_b64)
                .unwrap_or(false)
        },
    )
    .expect("POST");
    assert_eq!(status, 200, "reveal-share (TALLYING): expected 200, got {} body={:?}", status, json);
    assert_eq!(json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1), 0);
    block_wait();

    // ----- Step 10: Accumulated tally matches HomomorphicAdd(share0, share1) -----
    log_step("Step 10", "accumulated tally matches HomomorphicAdd(share0, share1)");
    let dec0 = base64::engine::general_purpose::STANDARD
        .decode(&enc_share_0)
        .expect("decode enc_share_0");
    let dec1 = base64::engine::general_purpose::STANDARD
        .decode(&enc_share_1)
        .expect("decode enc_share_1");
    let ct0 = elgamal::unmarshal(&dec0).expect("unmarshal ct0");
    let ct1 = elgamal::unmarshal(&dec1).expect("unmarshal ct1");
    let expected_accumulated = homomorphic_add(&ct0, &ct1);
    let expected_accumulated_b64 = ciphertext_to_base64(&expected_accumulated);

    let (status, json) = get_json(&format!("/zally/v1/tally/{}/0", round_id_hex)).expect("GET tally");
    assert_eq!(status, 200, "GET tally (step 10): expected 200, got {} body={:?}", status, json);
    let on_chain = json.get("tally").and_then(|t| t.get("1")).and_then(|v| v.as_str()).expect("tally[\"1\"]");
    assert_eq!(on_chain, expected_accumulated_b64, "accumulated ciphertext mismatch");

    // ----- Step 11: Submit tally finalizes -----
    log_step("Step 11", "submit tally finalizes");
    let tally_body = submit_tally_payload(
        &round_id,
        SESSION_CREATOR,
        &[TallyEntry { proposal_id: 0, vote_decision: 1, total_value: 2 }],
    );
    let (status, json) = post_json("/zally/v1/submit-tally", &tally_body).expect("POST");
    assert_eq!(status, 200, "submit-tally finalize: expected 200, got {} body={:?}", status, json);
    assert_eq!(json.get("code").and_then(|c| c.as_i64()).unwrap_or(-1), 0);
    block_wait();

    // ----- Step 12: Round FINALIZED -----
    log_step("Step 12", "round FINALIZED");
    let (_, json) = get_json(&format!("/zally/v1/round/{}", round_id_hex)).expect("GET round");
    assert_eq!(
        json.get("round").and_then(|r| r.get("status")).and_then(|s| s.as_i64()).unwrap(),
        SESSION_STATUS_FINALIZED
    );

    // ----- Step 13: Tally preserved -----
    log_step("Step 13", "tally preserved");
    let (_, json) = get_json(&format!("/zally/v1/tally/{}/0", round_id_hex)).expect("GET tally");
    assert_eq!(
        json.get("tally").and_then(|t| t.get("1")).and_then(|v| v.as_str()).unwrap(),
        expected_accumulated_b64
    );

    // ----- Step 14: Tally results queryable -----
    log_step("Step 14", "tally results queryable");
    let (status, json) = get_json(&format!("/zally/v1/tally-results/{}", round_id_hex))
        .expect("GET tally-results");
    assert_eq!(status, 200, "GET tally-results: expected 200, got {} body={:?}", status, json);
    let results = json.get("results").and_then(|r| r.as_array()).expect("results");
    assert!(!results.is_empty());
    assert_eq!(results[0].get("vote_decision").and_then(|v| v.as_u64()).unwrap_or(0), 1);
    assert_eq!(results[0].get("total_value").and_then(|v| v.as_u64()).unwrap_or(0), 2);

    log_step("Done", "voting flow happy path passed");
}
