#[allow(unused_imports)]
pub(crate) use crate::backend::pasta_curves;

use pasta_curves::{
    group::ff::{FromUniformBytes, PrimeField},
    pallas,
};
use subtle::ConstantTimeEq;
use zeroize::Zeroizing;

use crate::{
    action::derive_hotkey_x_coords_from_raw_address,
    delegation_capability::ValidatedDelegationCapabilityMaterialV1,
    governance::construct_van,
    hotkey::VOTING_HOTKEY_ADDRESS_INDEX,
    note_bundling::{BundlePolicy, ChunkResult},
    storage::queries,
    types::{validate_notes_for_round, NoteInfo, VotingError},
};

use super::{
    derivation::keyed_hash64, ValidatedRecoverableVotingRoundV1, VotingAuthorityRootBindingV1,
    VotingAuthorityRootV1,
};

const BUNDLE_BLINDING_DOMAIN: &[u8] = b"zcash_voting/self-custody-van-blinding/v1";
const BUNDLE_IDENTITY_DOMAIN: &[u8] = b"zcash_voting/self-custody-bundle-identity/v1";

/// Returns the immutable bundle policy selected by `recoverable-v1` rounds.
pub fn recoverable_bundle_policy_v1() -> BundlePolicy {
    crate::note_bundling::recoverable_bundle_policy_v1()
}

/// Note fields bound into one recoverable bundle identity.
///
/// This type deliberately omits `Debug` because the note value and the
/// association between a wallet's note and its public position are private.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RecoverableBundleNoteIdentityV1 {
    position: u64,
    commitment: [u8; 32],
    value: u64,
}

impl RecoverableBundleNoteIdentityV1 {
    pub fn position(&self) -> u64 {
        self.position
    }

    pub fn commitment(&self) -> &[u8; 32] {
        &self.commitment
    }

    pub fn value(&self) -> u64 {
        self.value
    }
}

/// Stable identity of one surviving bundle in the version 1 canonical plan.
///
/// This type deliberately omits `Debug` because it retains private note
/// identities.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct RecoverableBundleIdentityV1 {
    bundle_index: u32,
    notes: Vec<RecoverableBundleNoteIdentityV1>,
}

impl RecoverableBundleIdentityV1 {
    pub fn bundle_index(&self) -> u32 {
        self.bundle_index
    }

    pub fn notes(&self) -> &[RecoverableBundleNoteIdentityV1] {
        &self.notes
    }

    /// Canonical prefix-free encoding of the version, index, and real notes.
    pub fn canonical_transcript(&self) -> Vec<u8> {
        let mut fields = vec![
            vec![1],
            self.bundle_index.to_le_bytes().to_vec(),
            (self.notes.len() as u32).to_le_bytes().to_vec(),
        ];
        for note in &self.notes {
            fields.push(note.position.to_le_bytes().to_vec());
            fields.push(note.commitment.to_vec());
            fields.push(note.value.to_le_bytes().to_vec());
        }
        canonical_transcript(BUNDLE_IDENTITY_DOMAIN, &fields)
    }

    fn from_notes(bundle_index: u32, notes: &[NoteInfo]) -> Result<Self, VotingError> {
        let notes = notes
            .iter()
            .enumerate()
            .map(|(index, note)| {
                let commitment: [u8; 32] = note.commitment.as_slice().try_into().map_err(|_| {
                    VotingError::InvalidInput {
                        message: format!(
                            "recoverable bundle note {index} commitment must be 32 bytes, got {}",
                            note.commitment.len()
                        ),
                    }
                })?;
                Ok(RecoverableBundleNoteIdentityV1 {
                    position: note.position,
                    commitment,
                    value: note.value,
                })
            })
            .collect::<Result<Vec<_>, VotingError>>()?;
        Ok(Self {
            bundle_index,
            notes,
        })
    }

    pub(crate) fn matches_notes(&self, bundle_index: u32, notes: &[NoteInfo]) -> bool {
        Self::from_notes(bundle_index, notes)
            .map(|actual| actual == *self)
            .unwrap_or(false)
    }

    fn total_note_value(&self) -> Result<u64, VotingError> {
        self.notes.iter().try_fold(0u64, |total, note| {
            total
                .checked_add(note.value)
                .ok_or_else(|| VotingError::InvalidInput {
                    message: "recoverable bundle note value overflowed".to_string(),
                })
        })
    }

    fn derive_van_blinding(&self, root: &VotingAuthorityRootV1) -> RecoverableVanBlindingV1 {
        let fields = vec![
            root.context().canonical_transcript(),
            self.canonical_transcript(),
        ];
        let wide = keyed_hash64(root.secret_bytes(), BUNDLE_BLINDING_DOMAIN, &fields);
        let blinding = pallas::Base::from_uniform_bytes(&*wide).to_repr();
        RecoverableVanBlindingV1(Zeroizing::new(blinding))
    }
}

/// Exact typed bundle material required before a recoverable authority may vote.
///
/// Self-custody uses the canonical bundle identity produced by this module.
/// Custody uses one bundle from an authority-validated capability package.
/// This type deliberately omits `Debug` because the custody variant retains
/// privacy-sensitive capability material.
#[derive(Clone, Copy)]
pub enum RecoverableBundleMaterialV1<'a> {
    RecoverableSelfCustody(&'a RecoverableBundleIdentityV1),
    CustodyCapability {
        capability: &'a ValidatedDelegationCapabilityMaterialV1,
        bundle_index: u32,
    },
}

impl RecoverableBundleMaterialV1<'_> {
    pub(crate) fn bundle_index(self) -> u32 {
        match self {
            Self::RecoverableSelfCustody(identity) => identity.bundle_index(),
            Self::CustodyCapability { bundle_index, .. } => bundle_index,
        }
    }
}

/// One bound authority paired with an exact chain round and bundle material.
///
/// Wallets should reuse this value for vote signing.
/// Every operation revalidates the chain round and the
/// persisted bundle fields before using secret authority material.
#[derive(Clone, Copy)]
pub struct RecoverableBundleUseV1<'a> {
    authority_root: &'a VotingAuthorityRootV1,
    authority_binding: &'a VotingAuthorityRootBindingV1,
    current_round: &'a ValidatedRecoverableVotingRoundV1,
    bundle_material: RecoverableBundleMaterialV1<'a>,
}

impl<'a> RecoverableBundleUseV1<'a> {
    pub fn new(
        authority_root: &'a VotingAuthorityRootV1,
        authority_binding: &'a VotingAuthorityRootBindingV1,
        current_round: &'a ValidatedRecoverableVotingRoundV1,
        bundle_material: RecoverableBundleMaterialV1<'a>,
    ) -> Result<Self, VotingError> {
        authority_root.validate_binding(authority_binding)?;
        current_round.validate_authority_context(authority_root.context())?;
        Ok(Self {
            authority_root,
            authority_binding,
            current_round,
            bundle_material,
        })
    }

    pub(crate) fn authority_root(self) -> &'a VotingAuthorityRootV1 {
        self.authority_root
    }

    pub(crate) fn current_round(self) -> &'a ValidatedRecoverableVotingRoundV1 {
        self.current_round
    }

    pub(crate) fn bundle_index(self) -> u32 {
        self.bundle_material.bundle_index()
    }

    pub(crate) fn expected_material(
        self,
    ) -> Result<RecoverableExpectedBundleMaterialV1<'a>, VotingError> {
        self.authority_root
            .validate_binding(self.authority_binding)?;
        self.current_round
            .validate_authority_context(self.authority_root.context())?;
        expected_persisted_material(self.authority_root, self.bundle_material)
    }

    /// Checks the validated chain round against its stored row within the
    /// caller's database snapshot.
    pub(crate) fn validate_persisted_round_with_conn(
        self,
        conn: &rusqlite::Connection,
        wallet_id: &str,
        round_id: &str,
    ) -> Result<(), VotingError> {
        self.authority_root
            .validate_binding(self.authority_binding)?;
        self.current_round
            .validate_authority_context(self.authority_root.context())?;
        if hex::encode(self.authority_root.context().vote_round_id()) != round_id {
            return Err(bundle_material_mismatch());
        }
        let (stored_round, stored_network) =
            queries::load_round_params_with_network(conn, round_id, wallet_id)?;
        crate::storage::operations::validate_network_matches_round(
            stored_network,
            self.authority_root.context().network(),
            "recoverable bundle material",
        )?;
        if stored_round != *self.current_round.round_params() {
            return Err(round_parameters_mismatch());
        }
        Ok(())
    }

    /// Checks the stored bundle and round within the caller's database snapshot.
    pub(crate) fn validate_persisted_with_conn(
        self,
        conn: &rusqlite::Connection,
        wallet_id: &str,
        round_id: &str,
        bundle_index: u32,
    ) -> Result<(), VotingError> {
        self.validate_persisted_round_with_conn(conn, wallet_id, round_id)?;
        if self.bundle_material.bundle_index() != bundle_index {
            return Err(bundle_material_mismatch());
        }

        let expected = self.expected_material()?;
        let persisted = queries::load_persisted_recoverable_bundle_material(
            conn,
            round_id,
            wallet_id,
            bundle_index,
        )?;
        let persisted_blinding: [u8; 32] = persisted
            .van_comm_rand
            .as_slice()
            .try_into()
            .map_err(|_| bundle_material_mismatch())?;
        let persisted_van: [u8; 32] = persisted
            .gov_comm
            .as_slice()
            .try_into()
            .map_err(|_| bundle_material_mismatch())?;
        if !bool::from(persisted_blinding.ct_eq(&expected.van_blinding[..]))
            || !bool::from(persisted_van.ct_eq(&expected.van))
            || persisted.total_note_value != expected.total_note_value
            || persisted.address_index != VOTING_HOTKEY_ADDRESS_INDEX
            || expected.delegation_tx_hash.is_some()
                && persisted.delegation_tx_hash.as_deref() != expected.delegation_tx_hash
        {
            return Err(bundle_material_mismatch());
        }
        Ok(())
    }
}

pub(crate) struct RecoverableExpectedBundleMaterialV1<'a> {
    pub(crate) van_blinding: Zeroizing<[u8; 32]>,
    pub(crate) van: [u8; 32],
    pub(crate) total_note_value: u64,
    delegation_tx_hash: Option<&'a str>,
}

fn expected_persisted_material<'a>(
    root: &VotingAuthorityRootV1,
    material: RecoverableBundleMaterialV1<'a>,
) -> Result<RecoverableExpectedBundleMaterialV1<'a>, VotingError> {
    match material {
        RecoverableBundleMaterialV1::RecoverableSelfCustody(identity) => {
            let blinding = identity.derive_van_blinding(root);
            let hotkey = root.voting_hotkey()?;
            let (g_d_x, pk_d_x) =
                derive_hotkey_x_coords_from_raw_address(hotkey.raw_orchard_address())?;
            let total_note_value = identity.total_note_value()?;
            let van = construct_van(
                &g_d_x,
                &pk_d_x,
                total_note_value,
                root.context().vote_round_id(),
                blinding.as_bytes(),
            )?
            .try_into()
            .expect("construct_van returns 32 bytes");
            Ok(RecoverableExpectedBundleMaterialV1 {
                van_blinding: Zeroizing::new(*blinding.as_bytes()),
                van,
                total_note_value,
                delegation_tx_hash: None,
            })
        }
        RecoverableBundleMaterialV1::CustodyCapability {
            capability,
            bundle_index,
        } => {
            let hotkey = root.voting_hotkey()?;
            if capability.target() != &hotkey.delegation_target()
                || capability.vote_chain_id() != root.context().vote_chain_id()
                || capability.vote_round_id() != root.context().vote_round_id()
            {
                return Err(bundle_material_mismatch());
            }
            let bundle = capability
                .bundles()
                .iter()
                .find(|bundle| bundle.bundle_index() == bundle_index)
                .ok_or_else(bundle_material_mismatch)?;
            Ok(RecoverableExpectedBundleMaterialV1 {
                van_blinding: Zeroizing::new(*bundle.van_blinding()),
                van: *bundle.van_commitment(),
                total_note_value: bundle.total_note_value(),
                delegation_tx_hash: Some(bundle.delegation_tx_hash()),
            })
        }
    }
}

fn bundle_material_mismatch() -> VotingError {
    VotingError::InvalidInput {
        message: "persisted bundle material does not match the selected recoverable bundle"
            .to_string(),
    }
}

fn round_parameters_mismatch() -> VotingError {
    VotingError::InvalidInput {
        message: "persisted round parameters do not match the validated chain round".to_string(),
    }
}

/// One self-custody bundle and its deterministic recovery identity.
///
/// This type intentionally has no `Debug` or serialization implementation
/// because its `NoteInfo` rows include wallet secret material.
pub struct RecoverableSelfCustodyBundleV1 {
    identity: RecoverableBundleIdentityV1,
    notes: Vec<NoteInfo>,
}

impl RecoverableSelfCustodyBundleV1 {
    pub(crate) fn from_canonical_bundle(
        bundle_index: u32,
        notes: Vec<NoteInfo>,
    ) -> Result<Self, VotingError> {
        Ok(Self {
            identity: RecoverableBundleIdentityV1::from_notes(bundle_index, &notes)?,
            notes,
        })
    }

    pub fn identity(&self) -> &RecoverableBundleIdentityV1 {
        &self.identity
    }

    pub fn notes(&self) -> &[NoteInfo] {
        &self.notes
    }

    /// Derives the bundle's canonical VAN blinding from one matching round root.
    pub fn derive_van_blinding(&self, root: &VotingAuthorityRootV1) -> RecoverableVanBlindingV1 {
        self.identity.derive_van_blinding(root)
    }
}

fn canonical_transcript(domain: &[u8], fields: &[Vec<u8>]) -> Vec<u8> {
    let mut transcript = Vec::new();
    append_field(&mut transcript, domain);
    for field in fields {
        append_field(&mut transcript, field);
    }
    transcript
}

fn append_field(output: &mut Vec<u8>, field: &[u8]) {
    let len = u32::try_from(field.len()).expect("bundle identity fields fit u32");
    output.extend_from_slice(&len.to_le_bytes());
    output.extend_from_slice(field);
}

/// Complete self-custody plan under the frozen version 1 policy.
///
/// The plan intentionally has no `Debug` or serialization implementation
/// because its bundles retain the note secrets needed by delegation.
pub struct RecoverableSelfCustodyBundlePlanV1 {
    bundles: Vec<RecoverableSelfCustodyBundleV1>,
    eligible_weight: u64,
    dropped_count: usize,
    privacy_trim: crate::note_bundling::PrivacyTrim,
}

impl RecoverableSelfCustodyBundlePlanV1 {
    pub fn bundles(&self) -> &[RecoverableSelfCustodyBundleV1] {
        &self.bundles
    }

    pub fn bundle(&self, bundle_index: u32) -> Option<&RecoverableSelfCustodyBundleV1> {
        self.bundles
            .get(usize::try_from(bundle_index).ok()?)
            .filter(|bundle| bundle.identity.bundle_index == bundle_index)
    }

    pub fn eligible_weight(&self) -> u64 {
        self.eligible_weight
    }

    pub fn dropped_count(&self) -> usize {
        self.dropped_count
    }

    pub fn privacy_trim(&self) -> crate::note_bundling::PrivacyTrim {
        self.privacy_trim
    }
}

/// Applies the exact note planning rules frozen for `recoverable-v1`.
pub fn plan_recoverable_self_custody_bundles_v1(
    notes: &[NoteInfo],
) -> Result<RecoverableSelfCustodyBundlePlanV1, VotingError> {
    let notes = canonical_recoverable_notes(notes)?;
    let ChunkResult {
        bundles,
        eligible_weight,
        dropped_count,
        privacy_trim,
    } = crate::note_bundling::canonical_note_bundle_plan_for_notes(
        &notes,
        recoverable_bundle_policy_v1(),
    )?;
    let bundles = bundles
        .into_iter()
        .enumerate()
        .map(|(bundle_index, notes)| {
            let bundle_index = u32::try_from(bundle_index).map_err(|_| VotingError::Internal {
                message: "recoverable bundle index does not fit u32".to_string(),
            })?;
            RecoverableSelfCustodyBundleV1::from_canonical_bundle(bundle_index, notes)
        })
        .collect::<Result<Vec<_>, VotingError>>()?;
    Ok(RecoverableSelfCustodyBundlePlanV1 {
        bundles,
        eligible_weight,
        dropped_count,
        privacy_trim,
    })
}

/// Canonicalizes the recovery input before applying the unchanged legacy
/// packer. Normal wallet selections contain unique nullifiers, so this does not
/// change their bundle plan. Restore input order cannot influence which copy of
/// a duplicate survives, and conflicting rows fail closed.
pub(crate) fn canonical_recoverable_notes(
    notes: &[NoteInfo],
) -> Result<Vec<NoteInfo>, VotingError> {
    validate_notes_for_round(notes)?;
    let mut sorted = notes.to_vec();
    sorted.sort_by(|a, b| a.nullifier.cmp(&b.nullifier));

    let mut canonical: Vec<NoteInfo> = Vec::with_capacity(sorted.len());
    for note in sorted {
        if let Some(previous) = canonical.last() {
            if previous.nullifier == note.nullifier {
                if previous != &note {
                    return Err(VotingError::InvalidInput {
                        message:
                            "conflicting recoverable note rows share one private note identity"
                                .to_string(),
                    });
                }
                continue;
            }
        }
        canonical.push(note);
    }
    Ok(canonical)
}

/// Secret deterministic VAN blinding for one recoverable bundle.
///
/// This type is deliberately neither `Debug` nor `Serialize` and cannot be
/// constructed from arbitrary bytes by callers.
pub struct RecoverableVanBlindingV1(Zeroizing<[u8; 32]>);

impl RecoverableVanBlindingV1 {
    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{governance::BALLOT_DIVISOR, types::Network};

    fn note(value: u64, position: u64) -> NoteInfo {
        NoteInfo {
            commitment: vec![position as u8 + 1; 32],
            nullifier: vec![position as u8 + 17; 32],
            value,
            position,
            diversifier: vec![0; 11],
            rho: vec![0; 32],
            rseed: vec![0; 32],
            scope: 0,
            ufvk_str: String::new(),
        }
    }

    fn root() -> VotingAuthorityRootV1 {
        let context = crate::recoverable_authority::VotingAuthorityContextV1::from_fingerprint(
            Network::Testnet,
            0,
            [0x22; 32],
            "vote-chain-test",
            [0x01; 32],
        )
        .unwrap();
        let request = crate::recoverable_authority::SoftwareRegisteredKeyRequestV1::new(
            crate::recoverable_authority::RegisteredKeyApplicationV1::new(1),
            context,
        );
        VotingAuthorityRootV1::from_registered_key_output(&request, [0x55; 64])
    }

    #[test]
    fn recoverable_v1_policy_parameters_are_frozen() {
        let policy = recoverable_bundle_policy_v1();

        assert_eq!(policy.max_real_notes_per_bundle(), 5);
        assert_eq!(policy.bundle_addition_threshold(), None);
        assert_eq!(policy.max_privacy_bundles(), Some(2));
        assert_eq!(policy.privacy_drop_bps(), 100);
        assert_eq!(policy.max_privacy_drop_zatoshi(), Some(100_000_000_000));
    }

    #[test]
    fn recoverable_v1_percentage_budget_boundary_is_frozen() {
        // Ten large notes and five tail notes produce three full bundles. The
        // tail is exactly 1% of the selected value, then one zatoshi over it.
        let large_note_value = 990 * BALLOT_DIVISOR;
        let tail_note_value = 20 * BALLOT_DIVISOR;
        let mut at_budget = (0..10)
            .map(|position| note(large_note_value, position))
            .chain((10..15).map(|position| note(tail_note_value, position)))
            .collect::<Vec<_>>();
        let at = plan_recoverable_self_custody_bundles_v1(&at_budget).unwrap();

        at_budget[10].value += 1;
        let over = plan_recoverable_self_custody_bundles_v1(&at_budget).unwrap();

        assert_eq!(at.bundles().len(), 2);
        assert_eq!(at.eligible_weight(), 9_900 * BALLOT_DIVISOR);
        assert_eq!(at.privacy_trim().dropped_bundles, 1);
        assert_eq!(at.privacy_trim().dropped_notes, 5);
        assert_eq!(at.privacy_trim().dropped_value, 100 * BALLOT_DIVISOR);
        let surviving_positions = at
            .bundles()
            .iter()
            .map(|bundle| {
                bundle
                    .notes()
                    .iter()
                    .map(|note| note.position)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            surviving_positions,
            vec![
                (0_u64..5).collect::<Vec<_>>(),
                (5_u64..10).collect::<Vec<_>>()
            ]
        );

        assert_eq!(over.bundles().len(), 3);
        assert_eq!(over.privacy_trim().dropped_bundles, 0);
        assert_eq!(over.privacy_trim().dropped_value, 0);
    }

    #[test]
    fn recoverable_v1_absolute_drop_cap_boundary_is_frozen() {
        // Here 1% is above the absolute budget. The third bundle is retained
        // only after its value exceeds the frozen 1,000 ZEC cap by one zatoshi.
        let mut at_cap = (0..10)
            .map(|position| note(1_000_000_000_000, position))
            .chain((10..15).map(|position| note(20_000_000_000, position)))
            .collect::<Vec<_>>();
        let at = plan_recoverable_self_custody_bundles_v1(&at_cap).unwrap();

        at_cap[10].value += 1;
        let over = plan_recoverable_self_custody_bundles_v1(&at_cap).unwrap();

        assert_eq!(at.bundles().len(), 2);
        assert_eq!(at.privacy_trim().dropped_bundles, 1);
        assert_eq!(at.privacy_trim().dropped_notes, 5);
        assert_eq!(at.privacy_trim().dropped_value, 100_000_000_000);

        assert_eq!(over.bundles().len(), 3);
        assert_eq!(over.privacy_trim().dropped_bundles, 0);
        assert_eq!(over.privacy_trim().dropped_value, 0);
    }

    #[test]
    fn recoverable_plan_is_input_permutation_invariant() {
        let notes = (0..8)
            .map(|position| note(BALLOT_DIVISOR * (position + 1), position))
            .collect::<Vec<_>>();
        let mut shuffled = notes.clone();
        shuffled.rotate_left(3);
        shuffled.reverse();

        let expected = plan_recoverable_self_custody_bundles_v1(&notes).unwrap();
        let actual = plan_recoverable_self_custody_bundles_v1(&shuffled).unwrap();
        assert_eq!(expected.bundles.len(), actual.bundles.len());
        for (expected, actual) in expected.bundles.iter().zip(&actual.bundles) {
            assert!(expected.identity == actual.identity);
            assert_eq!(expected.notes, actual.notes);
        }
    }

    #[test]
    fn recoverable_plan_rejects_conflicting_duplicate_nullifier() {
        let first = note(BALLOT_DIVISOR, 0);
        let mut conflicting = first.clone();
        conflicting.value += 1;
        let error = plan_recoverable_self_custody_bundles_v1(&[first, conflicting])
            .err()
            .expect("conflicting duplicate must fail");
        assert!(error
            .to_string()
            .contains("conflicting recoverable note rows"));
        assert!(!error.to_string().contains(&hex::encode([17u8; 32])));
    }

    #[test]
    fn bundle_identity_and_blinding_are_deterministic() {
        let notes = (0..6)
            .map(|position| note(BALLOT_DIVISOR, position))
            .collect::<Vec<_>>();
        let plan = plan_recoverable_self_custody_bundles_v1(&notes).unwrap();
        let bundle = plan.bundle(0).unwrap();
        let root = root();
        let first = bundle.derive_van_blinding(&root);
        let second = bundle.derive_van_blinding(&root);

        assert_eq!(first.as_bytes(), second.as_bytes());
        assert_ne!(first.as_bytes(), &[0; 32]);
        assert_eq!(bundle.identity.bundle_index(), 0);
        assert_eq!(bundle.identity.notes().len(), 5);
    }

    #[test]
    fn bundle_identity_binds_real_note_fields_and_index() {
        let base = vec![note(BALLOT_DIVISOR, 0)];
        let plan = plan_recoverable_self_custody_bundles_v1(&base).unwrap();
        let identity = plan.bundle(0).unwrap().identity();
        assert!(identity.matches_notes(0, &base));

        let mut changed = base.clone();
        changed[0].position += 1;
        assert!(!identity.matches_notes(0, &changed));
        let mut changed = base.clone();
        changed[0].commitment[0] ^= 1;
        assert!(!identity.matches_notes(0, &changed));
        let mut changed = base;
        changed[0].value += 1;
        assert!(!identity.matches_notes(0, &changed));
        assert!(!identity.matches_notes(1, &changed));
    }
}
