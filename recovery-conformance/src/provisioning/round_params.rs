//! Reading a provisioned round back off the chain, bypassing config
//! authentication.
//!
//! A wallet normally learns a round's parameters from the signed dynamic
//! config: it fetches the document, verifies a `RoundAuthPayloadV2` signature
//! over the round id, `ea_pk`, and PIR layout, and only then trusts the values.
//! This suite deliberately skips all of that and reads the round from the chain
//! that created it.
//!
//! That is a sound trade *here* and nowhere else. Config authentication answers
//! "is this round genuine and endorsed", which is a question about trusting a
//! third party's document. The suite provisions the round itself, minutes
//! earlier, with its own coordinator key — so it already knows the answer, and
//! signing a document to verify a fact it just created would test the config
//! layer rather than recovery. Nothing here should be reused by a wallet, where
//! the signature is the entire point.
//!
//! What the suite does *not* skip is agreement: the parameters used to drive the
//! round come from the chain's own record, so a mistake in provisioning shows up
//! as a mismatch rather than being carried forward by a local copy.

use anyhow::{bail, Context, Result};
use zcash_voting::wire::VotingRoundParams;

/// Field numbers in the chain's `VoteRound` message.
///
/// Read positionally because the suite decodes the response itself rather than
/// linking the chain's protobuf definitions, which would pull a second,
/// independently-versioned copy of the message into this workspace.
mod field {
    pub const VOTE_ROUND_ID: u32 = 1;
    pub const SNAPSHOT_HEIGHT: u32 = 2;
    pub const NULLIFIER_IMT_ROOT: u32 = 6;
    pub const NC_ROOT: u32 = 7;
    pub const STATUS: u32 = 9;
    pub const EA_PK: u32 = 10;
}

/// `VoteRound.status` for a round whose ceremony has confirmed.
const STATUS_ACTIVE: u64 = 1;

/// A round as the chain records it.
pub struct ChainRound {
    pub params: VotingRoundParams,
    pub status: u64,
}

impl ChainRound {
    /// Whether the ceremony has confirmed and the round accepts submissions.
    ///
    /// A round is only usable once its DKG has produced an `ea_pk`; driving one
    /// before that fails during delegation, far from the cause.
    pub fn is_active(&self) -> bool {
        self.status == STATUS_ACTIVE
    }
}

/// Fetches a round's parameters from the vote chain by id.
///
/// Queries over the node's ABCI interface rather than the wallet-facing vote
/// server: this is provisioning, not submission, and the node is authoritative
/// for what it stored.
pub fn fetch_round(rpc_url: &str, round_id_hex: &str) -> Result<ChainRound> {
    let round_id = hex_bytes(round_id_hex).context("round id is not hex")?;
    let mut request = vec![
        0x0a,
        u8::try_from(round_id.len()).context("round id too long")?,
    ];
    request.extend_from_slice(&round_id);

    let url = format!(
        "{}/abci_query?path=%22%2Fsvote.v1.Query%2FVoteRound%22&data=0x{}",
        rpc_url.trim_end_matches('/'),
        to_hex(&request)
    );
    let body = curl(&url)?;
    let response: serde_json::Value =
        serde_json::from_slice(&body).context("ABCI response is not JSON")?;
    let inner = &response["result"]["response"];
    let code = inner["code"].as_u64().unwrap_or_default();
    if code != 0 {
        bail!(
            "chain refused the round query with code {code}: {}",
            inner["log"].as_str().unwrap_or("no log")
        );
    }
    let encoded = inner["value"]
        .as_str()
        .context("ABCI response carried no value")?;
    let bytes = base64_decode(encoded).context("ABCI value is not base64")?;

    // The response wraps the round in a single field.
    let (_, wrapped) = fields(&bytes)
        .into_iter()
        .find_map(|(number, value)| match value {
            Value::Bytes(bytes) => Some((number, bytes)),
            Value::Varint(_) => None,
        })
        .context("round query returned no round")?;

    let mut params = VotingRoundParams {
        vote_round_id: String::new(),
        snapshot_height: 0,
        ea_pk: Vec::new(),
        nc_root: Vec::new(),
        nullifier_imt_root: Vec::new(),
    };
    let mut status = 0;
    for (number, value) in fields(&wrapped) {
        match (number, value) {
            (field::VOTE_ROUND_ID, Value::Bytes(bytes)) => params.vote_round_id = to_hex(&bytes),
            (field::SNAPSHOT_HEIGHT, Value::Varint(height)) => params.snapshot_height = height,
            (field::NULLIFIER_IMT_ROOT, Value::Bytes(bytes)) => params.nullifier_imt_root = bytes,
            (field::NC_ROOT, Value::Bytes(bytes)) => params.nc_root = bytes,
            (field::STATUS, Value::Varint(value)) => status = value,
            (field::EA_PK, Value::Bytes(bytes)) => params.ea_pk = bytes,
            _ => {}
        }
    }

    if params.vote_round_id != round_id_hex {
        bail!(
            "chain returned round {} for a query about {round_id_hex}",
            params.vote_round_id
        );
    }
    if params.ea_pk.len() != 32 {
        bail!(
            "round {round_id_hex} has no election-authority key yet; its ceremony has not confirmed"
        );
    }
    Ok(ChainRound { params, status })
}

/// One decoded protobuf field value.
enum Value {
    Varint(u64),
    Bytes(Vec<u8>),
}

/// Decodes the top-level fields of a protobuf message.
///
/// Unknown fields and group wire types stop the walk rather than guessing: a
/// misparse that silently produced a shorter field list would hand back a round
/// missing exactly the value the caller needed.
fn fields(bytes: &[u8]) -> Vec<(u32, Value)> {
    let mut decoded = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let Some(key) = varint(bytes, &mut cursor) else {
            break;
        };
        let number = u32::try_from(key >> 3).unwrap_or_default();
        match key & 7 {
            0 => match varint(bytes, &mut cursor) {
                Some(value) => decoded.push((number, Value::Varint(value))),
                None => break,
            },
            2 => {
                let Some(length) = varint(bytes, &mut cursor).and_then(|l| usize::try_from(l).ok())
                else {
                    break;
                };
                let Some(end) = cursor.checked_add(length).filter(|end| *end <= bytes.len()) else {
                    break;
                };
                decoded.push((number, Value::Bytes(bytes[cursor..end].to_vec())));
                cursor = end;
            }
            _ => break,
        }
    }
    decoded
}

fn varint(bytes: &[u8], cursor: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0;
    loop {
        let byte = *bytes.get(*cursor)?;
        *cursor += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&text[index..index + 2], 16).ok())
        .collect()
}

fn base64_decode(text: &str) -> Option<Vec<u8>> {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut buffer = 0u32;
    let mut bits = 0;
    let mut out = Vec::new();
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        let index = TABLE.iter().position(|candidate| *candidate == byte)? as u32;
        buffer = (buffer << 6) | index;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Some(out)
}

fn curl(url: &str) -> Result<Vec<u8>> {
    let output = std::process::Command::new("curl")
        .args(["-sS", "--max-time", "30", url])
        .output()
        .with_context(|| format!("fetching {url}"))?;
    if !output.status.success() {
        bail!(
            "fetching the round failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}
