//! Smart note bundle planning for voting rounds.

use crate::{
    governance::{BALLOT_DIVISOR, BUNDLE_NOTE_SLOTS},
    types::{validate_notes_for_round, NoteInfo, SelectedNotes, VotingError},
};

/// Controls how many real wallet notes are placed into each voting bundle.
///
/// Bundles with fewer than [`BUNDLE_NOTE_SLOTS`] real notes are still padded
/// later by the proof construction path. This policy only controls how many
/// selected wallet notes can appear in one real bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BundlePolicy {
    max_real_notes_per_bundle: usize,
}

impl BundlePolicy {
    /// Builds a bundle policy with an explicit real-note capacity.
    ///
    /// # Errors
    ///
    /// Returns [`VotingError::InvalidInput`] when `max_real_notes_per_bundle` is
    /// outside `1..=BUNDLE_NOTE_SLOTS`.
    pub fn new(max_real_notes_per_bundle: usize) -> Result<Self, VotingError> {
        if (1..=BUNDLE_NOTE_SLOTS).contains(&max_real_notes_per_bundle) {
            Ok(Self {
                max_real_notes_per_bundle,
            })
        } else {
            Err(VotingError::InvalidInput {
                message: format!(
                    "max_real_notes_per_bundle must be in 1..={BUNDLE_NOTE_SLOTS}, got {max_real_notes_per_bundle}"
                ),
            })
        }
    }

    /// Builds a policy from an optional caller override.
    ///
    /// `None` selects the default policy, so SDK boundary layers can expose this
    /// as an optional setting without forcing most callers to think about it.
    pub fn from_optional_max_real_notes_per_bundle(
        max_real_notes_per_bundle: Option<u32>,
    ) -> Result<Self, VotingError> {
        match max_real_notes_per_bundle {
            Some(value) => {
                let value = usize::try_from(value).map_err(|_| VotingError::InvalidInput {
                    message: format!(
                        "max_real_notes_per_bundle must be in 1..={BUNDLE_NOTE_SLOTS}, got {value}"
                    ),
                })?;
                Self::new(value)
            }
            None => Ok(Self::default()),
        }
    }

    /// Returns the real-note capacity used by the bundler.
    pub fn max_real_notes_per_bundle(self) -> usize {
        self.max_real_notes_per_bundle
    }
}

impl Default for BundlePolicy {
    fn default() -> Self {
        Self {
            max_real_notes_per_bundle: BUNDLE_NOTE_SLOTS,
        }
    }
}

/// Result of value-aware note bundling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkResult {
    /// Surviving bundles, each with total >= BALLOT_DIVISOR.
    pub bundles: Vec<Vec<NoteInfo>>,
    /// Effective voting weight after per-bundle VAN quantization
    /// (each bundle contributes floor(total/BALLOT_DIVISOR) * BALLOT_DIVISOR).
    pub eligible_weight: u64,
    /// Number of notes that were dropped (in bundles below BALLOT_DIVISOR).
    pub dropped_count: usize,
}

/// Returns quantized zatoshi voting power for the selected note set.
pub fn voting_power(notes: &SelectedNotes) -> u64 {
    voting_power_with_policy(notes, BundlePolicy::default())
}

/// Returns quantized zatoshi voting power under an explicit bundle policy.
pub fn voting_power_with_policy(notes: &SelectedNotes, policy: BundlePolicy) -> u64 {
    let note_infos = notes.voting_note_infos();
    if validate_notes_for_round(&note_infos).is_err() {
        return 0;
    }
    chunk_notes_with_policy(&note_infos, policy).eligible_weight
}

/// Split notes into value-aware bundles using the default policy.
pub fn chunk_notes(notes: &[NoteInfo]) -> ChunkResult {
    chunk_notes_with_policy(notes, BundlePolicy::default())
}

/// Split notes into value-aware bundles using sequential packing.
///
/// Algorithm:
/// 1. Sort notes by value DESC, then position ASC as tiebreaker
/// 2. Fill bundles sequentially to policy capacity
/// 3. Drop bundles with total < BALLOT_DIVISOR
/// 4. Re-sort notes within each surviving bundle by position
/// 5. Sort surviving bundles by total value DESC (min position as tiebreaker)
///
/// Sequential packing concentrates high-value notes in early bundles, maximizing
/// per-bundle VAN weight and minimizing quantization loss. Dust notes naturally
/// end up in the last (smallest) bundle which gets dropped if below threshold.
/// Value-descending bundle order lets Keystone users sign the most valuable
/// bundles first and optionally skip the remaining low-value ones.
pub fn chunk_notes_with_policy(notes: &[NoteInfo], policy: BundlePolicy) -> ChunkResult {
    if notes.is_empty() {
        return ChunkResult {
            bundles: vec![],
            eligible_weight: 0,
            dropped_count: 0,
        };
    }

    // Step 1: Sort by value DESC, then position ASC as tiebreaker.
    let mut sorted = notes.to_vec();
    sorted.sort_by(|a, b| b.value.cmp(&a.value).then(a.position.cmp(&b.position)));

    // Step 2: Fill bundles sequentially to the configured real-note capacity.
    let mut bundle_notes: Vec<Vec<NoteInfo>> = Vec::new();
    let mut bundle_totals: Vec<u64> = Vec::new();
    let max_real_notes = policy.max_real_notes_per_bundle();

    for note in &sorted {
        if bundle_notes.is_empty() || bundle_notes.last().unwrap().len() >= max_real_notes {
            bundle_notes.push(Vec::new());
            bundle_totals.push(0);
        }
        let last = bundle_notes.len() - 1;
        bundle_totals[last] += note.value;
        bundle_notes[last].push(note.clone());
    }

    // Step 3: Drop bundles with total < BALLOT_DIVISOR.
    let total_notes: usize = bundle_notes.iter().map(|b| b.len()).sum();
    let mut surviving: Vec<(u64, Vec<NoteInfo>)> = Vec::new();
    let mut eligible_weight: u64 = 0;
    let mut surviving_notes: usize = 0;

    for (i, bundle) in bundle_notes.into_iter().enumerate() {
        if bundle_totals[i] >= BALLOT_DIVISOR {
            surviving_notes += bundle.len();
            eligible_weight += (bundle_totals[i] / BALLOT_DIVISOR) * BALLOT_DIVISOR;
            surviving.push((bundle_totals[i], bundle));
        }
    }
    let dropped_count = total_notes - surviving_notes;

    for (_, bundle) in &mut surviving {
        bundle.sort_by_key(|n| n.position);
    }

    // Sort surviving bundles by total value DESC. Use min position as a
    // deterministic tiebreaker for equal-value bundles.
    surviving.sort_by(|a, b| {
        b.0.cmp(&a.0).then_with(|| {
            let a_pos = a.1.first().map(|n| n.position).unwrap_or(u64::MAX);
            let b_pos = b.1.first().map(|n| n.position).unwrap_or(u64::MAX);
            a_pos.cmp(&b_pos)
        })
    });

    ChunkResult {
        bundles: surviving.into_iter().map(|(_, b)| b).collect(),
        eligible_weight,
        dropped_count,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::NoteRef;
    use zcash_client_backend::proto::service::TreeState;

    fn make_note(value: u64, position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            value,
            position,
            diversifier: vec![0; 11],
            rho: vec![0; 32],
            rseed: vec![0; 32],
            scope: 0,
            ufvk_str: String::new(),
        }
    }

    fn test_note_ref(value_zatoshi: u64, voting_weight_zatoshi: u64, position: u64) -> NoteRef {
        NoteRef {
            pool: "orchard".to_string(),
            txid_hex: hex::encode([position as u8; 32]),
            output_index: position as u32,
            value_zatoshi,
            voting_weight_zatoshi,
            commitment: vec![0x01; 32],
            nullifier: vec![0x02; 32],
            diversifier: vec![0x03; 11],
            rho: vec![0x04; 32],
            rseed: vec![0x05; 32],
            scope: 0,
            ufvk_str: String::new(),
            commitment_tree_position: position,
            mined_height: 1,
            anchor_height: 100,
        }
    }

    fn placeholder_tree_state(snapshot_height: u64) -> TreeState {
        TreeState {
            network: "test".to_string(),
            height: snapshot_height,
            hash: String::new(),
            time: 0,
            sapling_tree: String::new(),
            orchard_tree: String::new(),
            ironwood_tree: String::new(),
        }
    }

    #[test]
    fn voting_power_uses_smart_bundle_quantization() {
        let small_note_value = (BALLOT_DIVISOR / BUNDLE_NOTE_SLOTS as u64) + 1;
        let selected = SelectedNotes {
            notes: (0..BUNDLE_NOTE_SLOTS)
                .map(|position| {
                    test_note_ref(small_note_value, small_note_value, position as u64 + 1)
                })
                .collect(),
            snapshot_height: 100,
            anchor_tree_state: placeholder_tree_state(100),
        };

        assert_eq!(voting_power(&selected), BALLOT_DIVISOR);
    }

    #[test]
    fn voting_power_uses_custom_bundle_policy() {
        let selected = SelectedNotes {
            notes: (0..BUNDLE_NOTE_SLOTS)
                .map(|position| test_note_ref(13_000_000, 13_000_000, position as u64))
                .collect(),
            snapshot_height: 100,
            anchor_tree_state: placeholder_tree_state(100),
        };
        let policy = BundlePolicy::new(1).unwrap();

        assert_eq!(
            voting_power_with_policy(&selected, policy),
            BUNDLE_NOTE_SLOTS as u64 * BALLOT_DIVISOR
        );
    }

    #[test]
    fn voting_power_returns_zero_for_invalid_selected_notes() {
        let selected = SelectedNotes {
            notes: vec![NoteRef {
                commitment: vec![0x01; 31],
                ..test_note_ref(BALLOT_DIVISOR, BALLOT_DIVISOR, 1)
            }],
            snapshot_height: 100,
            anchor_tree_state: placeholder_tree_state(100),
        };

        assert_eq!(
            voting_power_with_policy(&selected, BundlePolicy::new(1).unwrap()),
            0
        );
    }

    #[test]
    fn test_chunk_notes_all_valid() {
        let notes: Vec<NoteInfo> = (0..BUNDLE_NOTE_SLOTS)
            .map(|i| make_note(13_000_000, i as u64))
            .collect();
        let result = chunk_notes(&notes);

        assert_eq!(result.bundles.len(), 1);
        assert_eq!(result.dropped_count, 0);
        let total = BUNDLE_NOTE_SLOTS as u64 * 13_000_000;
        assert_eq!(
            result.eligible_weight,
            (total / BALLOT_DIVISOR) * BALLOT_DIVISOR
        );
        assert_eq!(result.bundles[0].len(), BUNDLE_NOTE_SLOTS);
    }

    #[test]
    fn test_chunk_notes_dust_dropped() {
        let mut notes = vec![make_note(13_000_000, 0)];
        notes.extend((1..=BUNDLE_NOTE_SLOTS).map(|i| make_note(100, i as u64)));
        let result = chunk_notes(&notes);

        assert_eq!(result.bundles.len(), 1);
        assert_eq!(result.dropped_count, 1);
        assert_eq!(result.eligible_weight, 12_500_000);
        assert_eq!(result.bundles[0].len(), BUNDLE_NOTE_SLOTS);
    }

    #[test]
    fn test_chunk_notes_all_dust_empty() {
        let notes = vec![make_note(100, 0), make_note(200, 1), make_note(300, 2)];
        let result = chunk_notes(&notes);

        assert!(result.bundles.is_empty());
        assert_eq!(result.eligible_weight, 0);
        assert_eq!(result.dropped_count, 3);
    }

    #[test]
    fn test_chunk_notes_exact_threshold() {
        let notes = vec![make_note(BALLOT_DIVISOR, 0)];
        let result = chunk_notes(&notes);

        assert_eq!(result.bundles.len(), 1);
        assert_eq!(result.eligible_weight, BALLOT_DIVISOR);
        assert_eq!(result.dropped_count, 0);
    }

    #[test]
    fn test_chunk_notes_single_note() {
        let notes = vec![make_note(50_000_000, 42)];
        let result = chunk_notes(&notes);

        assert_eq!(result.bundles.len(), 1);
        assert_eq!(result.bundles[0].len(), 1);
        assert_eq!(result.bundles[0][0].position, 42);
        assert_eq!(result.eligible_weight, 50_000_000);
    }

    #[test]
    fn test_chunk_notes_deterministic() {
        let notes: Vec<NoteInfo> = (0..7)
            .map(|i| make_note(15_000_000 + i * 1_000_000, i))
            .collect();
        let r1 = chunk_notes(&notes);
        let r2 = chunk_notes(&notes);

        assert_eq!(r1.bundles.len(), r2.bundles.len());
        for (b1, b2) in r1.bundles.iter().zip(r2.bundles.iter()) {
            let p1: Vec<u64> = b1.iter().map(|n| n.position).collect();
            let p2: Vec<u64> = b2.iter().map(|n| n.position).collect();
            assert_eq!(p1, p2, "bundle positions must be deterministic");
        }
    }

    #[test]
    fn test_chunk_notes_position_ordering_within_bundles() {
        let notes = vec![
            make_note(20_000_000, 5),
            make_note(20_000_000, 1),
            make_note(20_000_000, 3),
            make_note(20_000_000, 7),
            make_note(20_000_000, 2),
        ];
        let result = chunk_notes(&notes);

        for bundle in &result.bundles {
            for window in bundle.windows(2) {
                assert!(
                    window[0].position < window[1].position,
                    "notes within bundle must be sorted by position"
                );
            }
        }
    }

    #[test]
    fn test_chunk_notes_bundles_sorted_by_value_desc() {
        let notes: Vec<NoteInfo> = (0..8).map(|i| make_note(15_000_000, i)).collect();
        let result = chunk_notes(&notes);

        assert_eq!(result.bundles.len(), 2);
        let totals: Vec<u64> = result
            .bundles
            .iter()
            .map(|b| b.iter().map(|n| n.value).sum())
            .collect();
        assert!(
            totals[0] >= totals[1],
            "bundle 0 total ({}) must be >= bundle 1 total ({})",
            totals[0],
            totals[1]
        );

        let min_positions: Vec<u64> = result
            .bundles
            .iter()
            .map(|b| b.first().unwrap().position)
            .collect();
        assert!(
            min_positions[0] < min_positions[1],
            "equal-total bundles should be ordered by min position"
        );
    }

    #[test]
    fn test_chunk_notes_largest_bundle_first() {
        let mut notes = Vec::new();
        for i in 0..BUNDLE_NOTE_SLOTS {
            notes.push(make_note(50_000_000, 10 + i as u64));
        }
        for i in 0..BUNDLE_NOTE_SLOTS {
            notes.push(make_note(13_000_000, i as u64));
        }
        let result = chunk_notes(&notes);

        assert_eq!(result.bundles.len(), 2);
        let total_0: u64 = result.bundles[0].iter().map(|n| n.value).sum();
        let total_1: u64 = result.bundles[1].iter().map(|n| n.value).sum();
        assert_eq!(total_0, BUNDLE_NOTE_SLOTS as u64 * 50_000_000);
        assert_eq!(total_1, BUNDLE_NOTE_SLOTS as u64 * 13_000_000);
        assert!(
            total_0 > total_1,
            "bundle 0 must have higher total than bundle 1"
        );
    }

    #[test]
    fn test_chunk_notes_empty() {
        let result = chunk_notes(&[]);

        assert!(result.bundles.is_empty());
        assert_eq!(result.eligible_weight, 0);
        assert_eq!(result.dropped_count, 0);
    }

    #[test]
    fn test_chunk_notes_default_capacity_per_bundle() {
        let notes: Vec<NoteInfo> = (0..12).map(|i| make_note(15_000_000, i)).collect();
        let result = chunk_notes(&notes);

        for bundle in &result.bundles {
            assert!(
                bundle.len() <= BUNDLE_NOTE_SLOTS,
                "bundle has {} notes, max is {}",
                bundle.len(),
                BUNDLE_NOTE_SLOTS
            );
        }
    }

    #[test]
    fn test_chunk_notes_one_real_note_per_bundle() {
        let notes: Vec<NoteInfo> = (0..BUNDLE_NOTE_SLOTS)
            .map(|i| make_note(13_000_000, i as u64))
            .collect();
        let policy = BundlePolicy::new(1).unwrap();

        let result = chunk_notes_with_policy(&notes, policy);

        assert_eq!(result.bundles.len(), BUNDLE_NOTE_SLOTS);
        assert_eq!(result.dropped_count, 0);
        assert_eq!(
            result.eligible_weight,
            BUNDLE_NOTE_SLOTS as u64 * BALLOT_DIVISOR
        );
        assert!(result.bundles.iter().all(|bundle| bundle.len() == 1));
    }

    #[test]
    fn test_chunk_notes_custom_capacity_drops_sub_threshold_tail() {
        let notes = vec![
            make_note(13_000_000, 0),
            make_note(13_000_000, 1),
            make_note(100, 2),
            make_note(100, 3),
            make_note(100, 4),
        ];
        let policy = BundlePolicy::new(2).unwrap();

        let result = chunk_notes_with_policy(&notes, policy);

        assert_eq!(result.bundles.len(), 1);
        assert_eq!(result.bundles[0].len(), 2);
        assert_eq!(result.dropped_count, 3);
        assert_eq!(result.eligible_weight, 25_000_000);
    }

    #[test]
    fn bundle_policy_rejects_invalid_real_note_capacity() {
        assert!(BundlePolicy::new(0).is_err());
        assert!(BundlePolicy::new(BUNDLE_NOTE_SLOTS + 1).is_err());
        assert!(BundlePolicy::from_optional_max_real_notes_per_bundle(None).is_ok());
        assert!(BundlePolicy::from_optional_max_real_notes_per_bundle(Some(999)).is_err());
    }
}
