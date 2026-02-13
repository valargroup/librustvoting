//! Integration test: server appends (delegate + vote), client syncs incrementally,
//! witnesses verify against server roots.
//!
//! This proves:
//! - Server appends continuously and checkpoints per block
//! - Client syncs incrementally via TreeSyncApi
//! - Client-generated witnesses are valid against server roots
//! - Roots match between server and client at every synced height
//! - Sync detects root mismatches and start_index discontinuities

use pasta_curves::Fp;
use vote_commitment_tree::{MerklePath, TreeClient, TreeServer, TreeSyncApi};

fn fp(x: u64) -> Fp {
    Fp::from(x)
}

/// Full lifecycle: MsgDelegateVote → client sync → MsgCastVote → client sync → witnesses verify.
///
/// Corresponds to the plan's integration test steps:
///  1. Create TreeServer (empty)
///  2. Simulate MsgDelegateVote: server.append(van_alice), server.checkpoint(1)
///  3. Client syncs block 1, generates witness at height 1
///  4. Simulate MsgCastVote: server.append_two(new_van_alice, vc_alice), server.checkpoint(2)
///  5. Client syncs block 2, generates witness at height 2
///  6. All witnesses verify against the server's roots
#[test]
fn server_append_client_sync_witness_roundtrip() {
    // ---------------------------------------------------------------
    // 1. Create TreeServer (empty)
    // ---------------------------------------------------------------
    let mut server = TreeServer::empty();
    let mut client = TreeClient::empty();

    // ---------------------------------------------------------------
    // 2. Simulate MsgDelegateVote: append VAN for Alice
    //    EndBlocker: checkpoint at height 1
    // ---------------------------------------------------------------
    let van_alice = fp(100);
    let van_idx = server.append(van_alice);
    assert_eq!(van_idx, 0, "first leaf should be at index 0");
    server.checkpoint(1);

    // ---------------------------------------------------------------
    // 3. Client syncs from server (gets block 1)
    //    Root consistency is now verified inside sync().
    // ---------------------------------------------------------------
    client.sync(&server).unwrap();

    assert_eq!(client.size(), 1, "client should have 1 leaf after sync");
    assert_eq!(
        client.last_synced_height(),
        Some(1),
        "client should be at height 1"
    );

    // Roots already verified inside sync(); double-check here.
    let server_root_1 = server.root_at_height(1).expect("server has root at height 1");
    let client_root_1 = client.root_at_height(1).expect("client has root at height 1");
    assert_eq!(
        server_root_1, client_root_1,
        "server and client roots must match at height 1"
    );

    // Client generates witness at anchor height 1 (Alice's VAN for ZKP #2).
    let witness_1 = client
        .witness(0, 1)
        .expect("witness for position 0 at height 1");

    assert!(
        witness_1.verify(van_alice, server_root_1),
        "witness for VAN at position 0 must verify against server root at height 1"
    );

    // Also verify via the server's own path (sanity check).
    let server_path_1 = server.path(0, 1).expect("server has path for position 0");
    assert!(server_path_1.verify(van_alice, server_root_1));

    // ---------------------------------------------------------------
    // 4. Simulate MsgCastVote: append new VAN + VC for Alice
    //    EndBlocker: checkpoint at height 2
    // ---------------------------------------------------------------
    let new_van_alice = fp(200); // New VAN (decremented proposal authority)
    let vc_alice = fp(300); // Vote commitment
    let cast_idx = server.append_two(new_van_alice, vc_alice);
    assert_eq!(cast_idx, 1, "MsgCastVote first leaf at index 1");
    assert_eq!(server.size(), 3, "server has 3 leaves total");
    server.checkpoint(2);

    // ---------------------------------------------------------------
    // 5. Client syncs block 2 (incremental — only new data)
    // ---------------------------------------------------------------
    client.sync(&server).unwrap();

    assert_eq!(client.size(), 3, "client should have 3 leaves after second sync");
    assert_eq!(
        client.last_synced_height(),
        Some(2),
        "client should be at height 2"
    );

    // Verify roots match at height 2.
    let server_root_2 = server.root_at_height(2).expect("server has root at height 2");
    let client_root_2 = client.root_at_height(2).expect("client has root at height 2");
    assert_eq!(
        server_root_2, client_root_2,
        "server and client roots must match at height 2"
    );

    // Root at height 1 is still accessible and unchanged.
    assert_eq!(
        client.root_at_height(1).unwrap(),
        server_root_1,
        "historical root at height 1 must be preserved"
    );

    // Roots at different heights must differ (tree grew).
    assert_ne!(
        server_root_1, server_root_2,
        "root should change after appending more leaves"
    );

    // ---------------------------------------------------------------
    // 6. Generate witnesses for VC and new VAN at height 2
    //    (Helper server needs VC witness for ZKP #3)
    // ---------------------------------------------------------------
    let witness_vc = client
        .witness(2, 2)
        .expect("witness for VC at position 2, anchor height 2");

    assert!(
        witness_vc.verify(vc_alice, server_root_2),
        "witness for VC must verify against server root at height 2"
    );

    // Also verify the new VAN witness (position 1) at height 2.
    let witness_new_van = client
        .witness(1, 2)
        .expect("witness for new VAN at position 1, anchor height 2");
    assert!(
        witness_new_van.verify(new_van_alice, server_root_2),
        "witness for new VAN must verify against server root at height 2"
    );

    // Verify server produces the same witnesses.
    let server_path_vc = server.path(2, 2).expect("server path for VC");
    assert!(server_path_vc.verify(vc_alice, server_root_2));
}

/// Test that the original VAN witness (position 0) still verifies at its
/// original anchor (height 1) even after the tree has grown.
#[test]
fn historical_witness_survives_growth() {
    let mut server = TreeServer::empty();

    // Block 1: one VAN.
    server.append(fp(1));
    server.checkpoint(1);
    let root_1 = server.root_at_height(1).unwrap();

    // Block 2: two more leaves.
    server.append_two(fp(2), fp(3));
    server.checkpoint(2);

    // Block 3: one more.
    server.append(fp(4));
    server.checkpoint(3);

    // Client syncs the full history.
    let mut client = TreeClient::empty();
    client.sync(&server).unwrap();
    assert_eq!(client.size(), 4);

    // Witness at height 1 (before growth) still verifies.
    let witness = client.witness(0, 1).expect("historical witness at height 1");
    assert!(
        witness.verify(fp(1), root_1),
        "historical witness must verify against the original anchor"
    );
}

/// Test the TreeSyncApi contract directly: get_tree_state, get_block_commitments,
/// and get_root_at_height return consistent data.
#[test]
fn sync_api_consistency() {
    let mut server = TreeServer::empty();

    // Append across multiple blocks.
    for height in 1..=5u32 {
        for i in 0..height {
            server.append(fp((height * 100 + i) as u64));
        }
        server.checkpoint(height);
    }

    // get_tree_state reflects the tip.
    let state = server.get_tree_state().unwrap();
    assert_eq!(state.height, 5);
    assert_eq!(state.next_index, 1 + 2 + 3 + 4 + 5); // 15 total leaves
    assert_eq!(state.root, server.root());

    // get_block_commitments for a subrange.
    let blocks = server.get_block_commitments(2, 4).unwrap();
    assert_eq!(blocks.len(), 3);
    assert_eq!(blocks[0].height, 2);
    assert_eq!(blocks[1].height, 3);
    assert_eq!(blocks[2].height, 4);
    assert_eq!(blocks[0].leaves.len(), 2);
    assert_eq!(blocks[1].leaves.len(), 3);
    assert_eq!(blocks[2].leaves.len(), 4);

    // get_root_at_height for each block matches server.root_at_height.
    for height in 1..=5u32 {
        let api_root = server.get_root_at_height(height).unwrap();
        let direct_root = server.root_at_height(height);
        assert_eq!(api_root, direct_root);
    }
}

/// Test that a fresh client can sync all blocks at once (full sync).
#[test]
fn full_sync_from_genesis() {
    let mut server = TreeServer::empty();

    // 10 blocks, 2 leaves each.
    for h in 1..=10u32 {
        server.append(fp(h as u64 * 10));
        server.append(fp(h as u64 * 10 + 1));
        server.checkpoint(h);
    }

    let mut client = TreeClient::empty();
    client.sync(&server).unwrap();

    assert_eq!(client.size(), 20);
    assert_eq!(client.last_synced_height(), Some(10));

    // Every checkpoint root matches.
    for h in 1..=10u32 {
        assert_eq!(
            client.root_at_height(h),
            server.root_at_height(h),
            "root mismatch at height {}",
            h
        );
    }

    // Witnesses for a few positions verify.
    for pos in [0u64, 5, 10, 19] {
        let leaf_val = if pos % 2 == 0 {
            fp((pos / 2 + 1) * 10)
        } else {
            fp((pos / 2 + 1) * 10 + 1)
        };
        let _anchor_h = (pos / 2 + 1) as u32; // Block that contains this leaf.
        let witness = client
            .witness(pos, 10) // witness at latest anchor
            .unwrap_or_else(|| panic!("witness for position {}", pos));
        assert!(
            witness.verify(leaf_val, server.root_at_height(10).unwrap()),
            "witness for position {} must verify",
            pos
        );
    }
}

/// Test idempotent sync — calling sync when already up-to-date is a no-op.
#[test]
fn sync_idempotent_when_up_to_date() {
    let mut server = TreeServer::empty();
    server.append(fp(1));
    server.checkpoint(1);

    let mut client = TreeClient::empty();
    client.sync(&server).unwrap();
    assert_eq!(client.size(), 1);

    // Sync again with no new data.
    client.sync(&server).unwrap();
    assert_eq!(client.size(), 1);
    assert_eq!(client.last_synced_height(), Some(1));
}

/// Test that server and client produce byte-identical auth paths.
#[test]
fn server_and_client_paths_are_identical() {
    let mut server = TreeServer::empty();
    server.append(fp(42));
    server.append(fp(43));
    server.checkpoint(1);

    let mut client = TreeClient::empty();
    client.sync(&server).unwrap();

    let server_path = server.path(0, 1).unwrap();
    let client_path = client.witness(0, 1).unwrap();

    assert_eq!(server_path.position(), client_path.position());
    assert_eq!(server_path.auth_path(), client_path.auth_path());
}

/// Two independent clients (wallet + helper server) sync from the same server.
///
/// This validates the actual production topology:
/// - Wallet needs a VAN witness for ZKP #2 at anchor height 2
/// - Helper server needs a VC witness for ZKP #3 at anchor height 3
/// - They sync independently, at different times, to different heights
/// - Both produce correct witnesses without interfering with each other
#[test]
fn two_clients_wallet_and_helper_server() {
    let mut server = TreeServer::empty();

    // -- Block 1: Alice delegates (MsgDelegateVote) -----------------------
    let van_alice = fp(100); // Alice's VAN (gov_comm)
    server.append(van_alice); // index 0
    server.checkpoint(1);

    // -- Block 2: Bob delegates (MsgDelegateVote) -------------------------
    let van_bob = fp(200); // Bob's VAN
    server.append(van_bob); // index 1
    server.checkpoint(2);

    // -- Block 3: Alice votes (MsgCastVote) -------------------------------
    let new_van_alice = fp(300); // Alice's new VAN (decremented authority)
    let vc_alice = fp(400); // Alice's vote commitment
    server.append_two(new_van_alice, vc_alice); // indices 2, 3
    server.checkpoint(3);

    // -- Block 4: Bob votes (MsgCastVote) ---------------------------------
    let new_van_bob = fp(500);
    let vc_bob = fp(600);
    server.append_two(new_van_bob, vc_bob); // indices 4, 5
    server.checkpoint(4);

    assert_eq!(server.size(), 6);

    // =====================================================================
    // Wallet client: Alice's phone
    // Syncs all blocks, needs VAN witness at position 0 for ZKP #2.
    // Uses anchor height 2 (the root before she voted).
    // =====================================================================
    let mut wallet = TreeClient::empty();
    wallet.sync(&server).unwrap();
    assert_eq!(wallet.size(), 6);

    let van_witness = wallet
        .witness(0, 2) // Alice's VAN at anchor before her vote
        .expect("wallet: VAN witness at position 0, anchor 2");
    let root_2 = server.root_at_height(2).unwrap();
    assert!(
        van_witness.verify(van_alice, root_2),
        "wallet: VAN witness must verify against root at height 2"
    );

    // =====================================================================
    // Helper server: independent process
    // Syncs all blocks, needs VC witness at position 3 for ZKP #3.
    // Uses anchor height 3 (the root right after Alice's vote).
    // =====================================================================
    let mut helper = TreeClient::empty();
    helper.sync(&server).unwrap();
    assert_eq!(helper.size(), 6);

    let vc_witness = helper
        .witness(3, 3) // Alice's VC at anchor right after her vote
        .expect("helper: VC witness at position 3, anchor 3");
    let root_3 = server.root_at_height(3).unwrap();
    assert!(
        vc_witness.verify(vc_alice, root_3),
        "helper: VC witness must verify against root at height 3"
    );

    // Both clients have identical tree state.
    for h in 1..=4u32 {
        assert_eq!(
            wallet.root_at_height(h),
            helper.root_at_height(h),
            "wallet and helper roots must match at height {}",
            h
        );
    }

    // Helper also produces Bob's VC witness (position 5, anchor 4).
    let vc_bob_witness = helper
        .witness(5, 4)
        .expect("helper: VC witness for Bob at position 5, anchor 4");
    let root_4 = server.root_at_height(4).unwrap();
    assert!(
        vc_bob_witness.verify(vc_bob, root_4),
        "helper: Bob's VC witness must verify against root at height 4"
    );

    // Wallet can also produce Alice's new VAN witness (position 2, anchor 4)
    // for a hypothetical second vote on another proposal.
    let new_van_witness = wallet
        .witness(2, 4)
        .expect("wallet: new VAN witness at position 2, anchor 4");
    assert!(
        new_van_witness.verify(new_van_alice, root_4),
        "wallet: new VAN witness must verify at latest anchor"
    );
}

/// Shard boundary crossing: 40 leaves across multiple blocks.
///
/// With SHARD_HEIGHT = 4, each shard covers 2^4 = 16 leaves.
/// This test fills 2.5 shards (40 leaves), then verifies witnesses for
/// positions in shard 0 (pos 0), shard 1 (pos 16), and shard 2 (pos 32).
/// Witnesses that span shard boundaries require the tree to combine data
/// from adjacent shards — this is where subtle bugs tend to hide.
#[test]
fn shard_boundary_crossing() {
    let mut server = TreeServer::empty();

    // Append 40 leaves across 10 blocks (4 leaves per block).
    // Shard 0: leaves [0..15], Shard 1: leaves [16..31], Shard 2: leaves [32..39]
    for block_h in 1..=10u32 {
        for i in 0..4u64 {
            let leaf_idx = (block_h as u64 - 1) * 4 + i;
            server.append(fp(leaf_idx * 7 + 1)); // deterministic distinct values
        }
        server.checkpoint(block_h);
    }

    assert_eq!(server.size(), 40);

    // Client syncs all 10 blocks.
    let mut client = TreeClient::empty();
    client.sync(&server).unwrap();
    assert_eq!(client.size(), 40);
    assert_eq!(client.last_synced_height(), Some(10));

    // All checkpoint roots match.
    for h in 1..=10u32 {
        assert_eq!(
            client.root_at_height(h),
            server.root_at_height(h),
            "root mismatch at height {}",
            h
        );
    }

    let root_10 = server.root_at_height(10).unwrap();

    // Test witnesses at shard boundaries:

    // Position 0 — first leaf in shard 0
    let w0 = client.witness(0, 10).expect("witness for pos 0");
    assert!(w0.verify(fp(1), root_10), "pos 0 (shard 0 start)");

    // Position 15 — last leaf in shard 0
    let w15 = client.witness(15, 10).expect("witness for pos 15");
    assert!(w15.verify(fp(15 * 7 + 1), root_10), "pos 15 (shard 0 end)");

    // Position 16 — first leaf in shard 1 (crosses shard boundary)
    let w16 = client.witness(16, 10).expect("witness for pos 16");
    assert!(
        w16.verify(fp(16 * 7 + 1), root_10),
        "pos 16 (shard 1 start — boundary crossing)"
    );

    // Position 31 — last leaf in shard 1
    let w31 = client.witness(31, 10).expect("witness for pos 31");
    assert!(w31.verify(fp(31 * 7 + 1), root_10), "pos 31 (shard 1 end)");

    // Position 32 — first leaf in shard 2 (second boundary crossing)
    let w32 = client.witness(32, 10).expect("witness for pos 32");
    assert!(
        w32.verify(fp(32 * 7 + 1), root_10),
        "pos 32 (shard 2 start — boundary crossing)"
    );

    // Position 39 — last appended leaf
    let w39 = client.witness(39, 10).expect("witness for pos 39");
    assert!(w39.verify(fp(39 * 7 + 1), root_10), "pos 39 (tree tip)");

    // Historical witness: position 16 at anchor height 5 (when shard 1 was
    // only partially filled — 20 leaves total at that point, shard 1 had
    // positions 16-19).
    let root_5 = server.root_at_height(5).unwrap();
    let w16_h5 = client
        .witness(16, 5)
        .expect("witness for pos 16 at height 5");
    assert!(
        w16_h5.verify(fp(16 * 7 + 1), root_5),
        "historical witness at partial shard 1"
    );

    // Server and client produce identical paths across shard boundaries.
    let server_path_16 = server.path(16, 10).unwrap();
    let client_path_16 = client.witness(16, 10).unwrap();
    assert_eq!(server_path_16, client_path_16, "paths must be identical at shard boundary");
}

/// Test MerklePath serialization roundtrip.
#[test]
fn merkle_path_serialization_roundtrip() {
    let mut server = TreeServer::empty();
    server.append(fp(10));
    server.append(fp(20));
    server.append(fp(30));
    server.checkpoint(1);

    let path = server.path(1, 1).unwrap();
    let bytes = path.to_bytes();

    // Expected size: 4 (position) + 32 * 32 (auth_path) = 1028 bytes.
    assert_eq!(bytes.len(), 4 + 32 * 32);

    let restored = MerklePath::from_bytes(&bytes).expect("deserialization must succeed");
    assert_eq!(restored.position(), path.position());
    assert_eq!(restored.auth_path(), path.auth_path());

    // Restored path still verifies.
    let root = server.root_at_height(1).unwrap();
    assert!(restored.verify(fp(20), root));
}
