use std::env;

use anyhow::Result;
use ff::PrimeField as _;
use orchard::note::ExtractedNoteCommitment;
use orchard::vote::calculate_merkle_paths;
use pasta_curves::Fp;
use rusqlite::Connection;

use zcash_vote::trees::build_nf_ranges;

/// Find the gap-range index that contains `value`.
/// Returns `Some(i)` where `ranges[2*i]` is the low bound and `ranges[2*i+1]`
/// is the high bound (inclusive), or `None` if the value is not in any gap range
/// (i.e. it IS an existing nullifier boundary).
fn find_range_for_value(ranges: &[Fp], value: Fp) -> Option<usize> {
    let num_ranges = ranges.len() / 2;
    for i in 0..num_ranges {
        let low = ranges[2 * i];
        let high = ranges[2 * i + 1];
        if value >= low && value <= high {
            return Some(i);
        }
    }
    None
}

fn main() -> Result<()> {
    let db_path = env::var("DB_PATH").unwrap_or_else(|_| "nullifiers.db".to_string());

    println!("Opening database: {}", db_path);
    let connection = Connection::open(&db_path)?;

    // ── 1. Load raw nullifiers ──────────────────────────────────────────
    println!("Loading nullifiers from nfs table...");
    let mut stmt = connection.prepare("SELECT hash FROM nfs")?;
    let rows = stmt.query_map([], |r| {
        let v = r.get::<_, [u8; 32]>(0)?;
        let v = Fp::from_repr(v).unwrap();
        Ok(v)
    })?;
    let mut raw_nfs: Vec<Fp> = rows.collect::<std::result::Result<Vec<_>, _>>()?;
    raw_nfs.sort();
    println!("  Loaded {} nullifiers", raw_nfs.len());

    // ── 2. Build gap ranges ─────────────────────────────────────────────
    let ranges = build_nf_ranges(raw_nfs.iter().copied());
    let num_ranges = ranges.len() / 2;
    println!("  Built {} gap ranges ({} leaves)", num_ranges, ranges.len());

    // ── 3. Compute Merkle root (no paths yet) ───────────────────────────
    println!("Computing Merkle root over range leaves...");
    let (root, _) = calculate_merkle_paths(0, &[], &ranges);
    println!("  Root: {:?}", hex::encode(root.to_repr()));

    // ══════════════════════════════════════════════════════════════════════
    //  TEST 1: Non-existing nullifier  →  exclusion path SHOULD succeed
    // ══════════════════════════════════════════════════════════════════════
    println!("\n── TEST 1: Non-inclusion proof for a NON-EXISTING value ──");

    // Pick a value that is guaranteed to be in the first gap range.
    // The first range is [0, first_nf - 1].  We use Fp::zero().
    let test_value = Fp::zero();
    println!("  Test value: 0x{}", hex::encode(test_value.to_repr()));

    let range_idx = find_range_for_value(&ranges, test_value);
    match range_idx {
        Some(idx) => {
            let low = ranges[2 * idx];
            let high = ranges[2 * idx + 1];
            println!(
                "  Found in range {}: [0x{}..0x{}]",
                idx,
                hex::encode(low.to_repr()),
                hex::encode(high.to_repr())
            );

            // Get Merkle paths for the low and high leaves of this range
            let pos_low = (2 * idx) as u32;
            let pos_high = (2 * idx + 1) as u32;
            let (root2, paths) =
                calculate_merkle_paths(0, &[pos_low, pos_high], &ranges);

            // Sanity: root must be the same
            assert_eq!(root, root2, "Root mismatch between calls");

            // Verify each Merkle path reconstructs to the root
            for (i, path) in paths.iter().enumerate() {
                let mp = path.to_orchard_merkle_tree();
                let anchor = mp.root(
                    ExtractedNoteCommitment::from_bytes(&path.value.to_repr()).unwrap(),
                );
                assert_eq!(
                    root.to_repr(),
                    anchor.to_bytes(),
                    "Merkle path {} does not reconstruct to root",
                    i
                );
                println!(
                    "  Merkle path {} verified (position {}, value 0x{})",
                    i,
                    path.position,
                    hex::encode(path.value.to_repr())
                );
            }

            // Confirm test_value is in [low, high]
            assert!(test_value >= low && test_value <= high);

            println!("  PASS: Non-inclusion proof SUCCEEDED — value is in a gap range with valid Merkle paths");
        }
        None => {
            panic!("BUG: Fp::zero() was not found in any gap range — unexpected");
        }
    }

    // ══════════════════════════════════════════════════════════════════════
    //  TEST 2: Existing nullifier  →  exclusion path SHOULD fail
    // ══════════════════════════════════════════════════════════════════════
    println!("\n── TEST 2: Non-inclusion proof for an EXISTING nullifier ──");

    let existing_nf = raw_nfs[0]; // first nullifier (smallest)
    println!(
        "  Existing nullifier: 0x{}",
        hex::encode(existing_nf.to_repr())
    );

    let result = find_range_for_value(&ranges, existing_nf);
    assert!(
        result.is_none(),
        "BUG: existing nullifier was found inside a gap range!"
    );
    println!("  PASS: Existing nullifier correctly NOT found in any gap range");
    println!("    -> Cannot construct a non-inclusion proof (as expected)");

    // ══════════════════════════════════════════════════════════════════════
    //  TEST 3: Another non-existing value (middle of a later range)
    // ══════════════════════════════════════════════════════════════════════
    println!("\n── TEST 3: Non-inclusion proof for a value in a later gap range ──");

    // Pick a value from the range at index num_ranges/2
    let mid_range = num_ranges / 2;
    let mid_low = ranges[2 * mid_range];
    // Use mid_low + 1 as our test value (safe as long as range has size > 1)
    let test_value_2 = mid_low + Fp::one();
    println!(
        "  Test value: 0x{} (low+1 of range {})",
        hex::encode(test_value_2.to_repr()),
        mid_range
    );

    let range_idx_2 = find_range_for_value(&ranges, test_value_2);
    match range_idx_2 {
        Some(idx) => {
            assert_eq!(idx, mid_range);
            let pos_low = (2 * idx) as u32;
            let pos_high = (2 * idx + 1) as u32;
            let (root3, paths) =
                calculate_merkle_paths(0, &[pos_low, pos_high], &ranges);
            assert_eq!(root, root3, "Root mismatch");

            for path in paths.iter() {
                let mp = path.to_orchard_merkle_tree();
                let anchor = mp.root(
                    ExtractedNoteCommitment::from_bytes(&path.value.to_repr()).unwrap(),
                );
                assert_eq!(root.to_repr(), anchor.to_bytes());
            }

            assert!(test_value_2 >= ranges[2 * idx] && test_value_2 <= ranges[2 * idx + 1]);
            println!(
                "  PASS: Non-inclusion proof SUCCEEDED for range {} with valid Merkle paths",
                idx
            );
        }
        None => {
            panic!("BUG: test value in middle of a gap range was not found");
        }
    }

    println!("\n== All tests passed ==");
    Ok(())
}
