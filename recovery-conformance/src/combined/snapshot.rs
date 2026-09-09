use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};

/// Public recovery metadata for one member, plus its durable confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CombinedMember {
    pub proposal_id: u32,
    pub batch_digest: Option<Vec<u8>>,
    pub batch_index: Option<u32>,
    pub batch_size: Option<u32>,
    pub combined: bool,
    pub anchor_height: u32,
    pub position: Option<u64>,
    pub tx_hash: Option<String>,
    pub has_plan: bool,
}

/// A wallet/round/bundle snapshot containing no secret payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CombinedBundle {
    pub wallet_id: String,
    pub round_id: String,
    pub bundle_index: u32,
    pub pczt_fingerprint: Option<String>,
    pub proof_fingerprint: Option<String>,
    pub van_position: Option<u64>,
    pub delegation_hash: Option<String>,
    /// Batch digest and a fingerprint binding its stored delegation authorization.
    pub authorizations: Vec<(Vec<u8>, String)>,
    pub members: Vec<CombinedMember>,
}

#[derive(serde::Deserialize)]
struct RecoveryMetadata {
    vote_round_id: String,
    bundle_index: u32,
    proposal_id: u32,
    batch_digest: Option<Vec<u8>>,
    batch_index: Option<u32>,
    batch_size: Option<u32>,
    delegation_van: Option<[u8; 32]>,
    anchor_height: u32,
}

fn fingerprint(bytes: Option<Vec<u8>>) -> Option<String> {
    bytes.map(|bytes| format!("{:x}", Sha256::digest(bytes)))
}

impl CombinedBundle {
    /// Reads all bundles through the caller's read transaction, scoping every
    /// child query to the complete wallet/round/bundle identity.
    pub fn read_all(connection: &Connection) -> Result<Vec<Self>> {
        let mut statement = connection.prepare(
            "select b.wallet_id, b.round_id, b.bundle_index, b.pczt_sighash,
                    p.proof, b.van_leaf_position, b.delegation_tx_hash
             from bundles b left join proofs p using(wallet_id, round_id, bundle_index)
             order by b.wallet_id, b.round_id, b.bundle_index",
        )?;
        let mut bundles = statement
            .query_map([], |row| {
                Ok(Self {
                    wallet_id: row.get(0)?,
                    round_id: row.get(1)?,
                    bundle_index: row.get(2)?,
                    pczt_fingerprint: fingerprint(row.get(3)?),
                    proof_fingerprint: fingerprint(row.get(4)?),
                    van_position: row.get(5)?,
                    delegation_hash: row.get(6)?,
                    authorizations: Vec::new(),
                    members: Vec::new(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for bundle in &mut bundles {
            let scope = params![bundle.wallet_id, bundle.round_id, bundle.bundle_index];
            let mut authorization = connection.prepare(
                "select batch_digest, delegation_generation_digest, spend_auth_signature
                 from delegate_cast_recovery where wallet_id=?1 and round_id=?2 and bundle_index=?3
                 order by batch_digest",
            )?;
            bundle.authorizations = authorization
                .query_map(scope, |row| {
                    let generation: Vec<u8> = row.get(1)?;
                    let signature: Vec<u8> = row.get(2)?;
                    let mut digest = Sha256::new();
                    digest.update(generation);
                    digest.update(signature);
                    Ok((row.get(0)?, format!("{:x}", digest.finalize())))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut votes = connection.prepare(
                "select v.proposal_id, v.commitment_bundle_json, v.vc_tree_position, v.tx_hash,
                    exists(select 1 from helper_share_plans h where h.wallet_id=v.wallet_id
                    and h.round_id=v.round_id and h.bundle_index=v.bundle_index and h.proposal_id=v.proposal_id
                    and h.commitment_bundle_json=v.commitment_bundle_json)
                 from votes v where v.wallet_id=?1 and v.round_id=?2 and v.bundle_index=?3
                 order by v.proposal_id")?;
            let mut rows = votes.query(scope)?;
            while let Some(row) = rows.next()? {
                let proposal_id: u32 = row.get(0)?;
                let recovery: Option<String> = row.get(1)?;
                let metadata = recovery
                    .map(|json| serde_json::from_str::<RecoveryMetadata>(&json))
                    .transpose()
                    .context("invalid member recovery metadata")?;
                if let Some(metadata) = &metadata {
                    anyhow::ensure!(
                        metadata.vote_round_id == bundle.round_id
                            && metadata.bundle_index == bundle.bundle_index
                            && metadata.proposal_id == proposal_id,
                        "recovery member belongs to a different round, bundle or proposal"
                    );
                }
                bundle.members.push(CombinedMember {
                    proposal_id,
                    batch_digest: metadata.as_ref().and_then(|m| m.batch_digest.clone()),
                    batch_index: metadata.as_ref().and_then(|m| m.batch_index),
                    batch_size: metadata.as_ref().and_then(|m| m.batch_size),
                    combined: metadata
                        .as_ref()
                        .is_some_and(|m| m.delegation_van.is_some()),
                    anchor_height: metadata.as_ref().map_or(0, |m| m.anchor_height),
                    position: row.get(2)?,
                    tx_hash: row.get(3)?,
                    has_plan: row.get(4)?,
                });
            }
            bundle.members.sort_by_key(|member| member.batch_index);
        }
        Ok(bundles)
    }
}
