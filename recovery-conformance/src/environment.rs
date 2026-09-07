//! Where the suite gets its staging endpoints, its keys, and its wallet.
//!
//! Everything here is resolved and validated once, before a round is
//! provisioned or a proof is started. A run that is going to fail on a missing
//! credential should fail in the first second, not forty minutes in with a
//! delegation half-registered on the vote chain.
//!
//! # Credentials
//!
//! No secret is read from a file in this repository, embedded in a binary, or
//! passed on a command line. Values come from the environment at run time, and
//! the environment is populated by Infisical.
//!
//! | What | Where |
//! | --- | --- |
//! | Vote-manager key for `svote-1` round creation | Infisical `vote` project (`40862c6d-a089-4355-b405-0477be0ee3b1`), key `VOTE_MANAGER_VOTE_SDK`, present in `dev`/`staging`/`prod`; the suite uses **`staging`** |
//! | Voting wallet seed (fixed across runs) | Infisical, same project, key `VOTE_SDK_VOTER_TEST`, present in `dev`/`staging`/`prod` |
//!
//! `VOTE_MANAGER_VOTE_SDK` is a **12-word BIP39 mnemonic**, not a raw key —
//! unlike the older `VOTE_MANAGER`, which is a 64-character hex seed. Derive
//! the signer with the cosmos defaults, since `svote-1` overrides none of
//! them: secp256k1 over BIP44 path `m/44'/118'/0'/0/0`, with account addresses
//! bech32-encoded under the `sv` prefix (`app/app_config.go` sets
//! `SetBech32PrefixForAccount("sv", "svpub")`).
//!
//! `VOTE_SDK_VOTER_TEST` is the fixed voter's **24-word BIP39 mnemonic** — a
//! Zcash wallet seed, derived through ZIP-32, not a cosmos key. It is the
//! wallet whose notes every round delegates.
//!
//! Note the three credentials have three different shapes, so none of the
//! parsing paths is reusable: `VOTE_SDK_VOTER_TEST` is 24 words,
//! `VOTE_MANAGER_VOTE_SDK` is 12, and the older `VOTE_MANAGER` is a 64-char
//! hex seed.
//!
//! The same wallet is reused across every round, and it never needs
//! re-funding: TX1 is a PCZT-only signing artifact that is never broadcast to
//! Zcash, so no note is ever spent there. What *is* consumed is the delegation
//! on the vote chain, and only per round — gov nullifiers are domain-separated
//! by `vote_round_id`, so a fresh round frees the same notes again.
//!
//! `VOTE_MANAGER_VOTE_SDK` is the scoped coordinator key: it authorizes
//! `MsgCreateVotingSession`, which the chain restricts to the vote manager
//! (`ValidateVoteManagerOnly`). It is not the attestation key — the suite
//! self-signs the dynamic config it trusts rather than asking anyone to
//! attest a throwaway round.
//!
//! Endpoints are not configured here. They are read from the published staging
//! configuration, the same document a staging wallet reads; see
//! [`stage_config`](crate::stage_config).
//!
//! Run the suite under Infisical so the process inherits the values without
//! any of them touching disk:
//!
//! ```text
//! infisical run --env=staging -- make recovery-conformance
//! ```
//!
//! To check a key is present without printing it, test the value against the
//! literal `*not found*`. Do **not** test the exit code: `infisical secrets
//! get` returns 0 for a key that does not exist and substitutes that
//! placeholder, so an `rc`-based check reports every key as present — a
//! configuration error would then surface as a signing failure deep inside a
//! provisioning run rather than at startup.
//!
//! ```text
//! infisical secrets get VOTE_MANAGER_VOTE_SDK --env=staging -o json \
//!   | grep -q '"secretValue":"\*not found\*"' && echo absent || echo present
//! ```
//!
//! Note that `~/agent-global.env` may carry a namespaced `VOTE_DEV__*` copy
//! from a previous sync. That file is a point-in-time snapshot and goes stale
//! whenever a key is added or rotated; Infisical is the source of truth, and
//! this suite reads the live values rather than that cache.

/// Infisical environment the suite reads its credentials from.
///
/// The key exists in `dev`, `staging` and `prod`. `staging` is the matching
/// scope: this key provisions rounds on staging infrastructure, and reading it
/// from `prod` would let a misconfigured run point production credentials at a
/// chain that is about to be deliberately crashed.
pub const INFISICAL_ENVIRONMENT: &str = "staging";

/// BIP44 derivation path for the coordinator signer.
///
/// Coin type 133 is Zcash's. Verified against the chain: this path reproduces
/// the registered coordinator, and 118, 60 and 1 do not.
pub const VOTE_MANAGER_DERIVATION_PATH: &str = "m/44'/133'/0'/0/0";

/// Bech32 human-readable prefix for `svote-1` account addresses.
pub const VOTE_CHAIN_ADDRESS_PREFIX: &str = "sv";

/// Name of the environment variable carrying the fixed voter's wallet seed.
///
/// A 24-word BIP39 mnemonic read into memory at run time and never written to
/// disk by this suite. The notes it holds are what every round delegates.
///
/// The wallet holds 11 notes, which bundle into 3: notes pack five to a bundle
/// (`BUNDLE_NOTE_SLOTS`) as 5/5/1, and the privacy trim cannot shed the last
/// one because a single note of a near-equal set is far outside the 1% drop
/// budget. The third bundle is what gives the multi-bundle invariants a bundle
/// to spare, so a rebalance that changes this layout weakens them silently —
/// provisioning asserts the count rather than assuming it.
pub const VOTER_SEED_VAR: &str = "VOTE_SDK_VOTER_TEST";

/// Name of the environment variable carrying the scoped vote-manager key.
///
/// Held as a name, never a value: the key is read from the process environment
/// at run time so it is never written to a file, a log, or an argv this suite
/// controls.
pub const VOTE_MANAGER_KEY_VAR: &str = "VOTE_MANAGER_VOTE_SDK";

/// Lightwalletd endpoints for the Zcash chain the staging deployment indexes.
///
/// Testnet. Verified by asking the servers themselves: `testnet.zec.rocks`
/// reports `chainName: "test"`, and its tip sits just above the staging PIR
/// snapshot height.
///
/// Stardust is deliberately **not** listed. Every Stardust host is mainnet
/// (`us|eu|eu2|jp.zec.stardust.rest` all report `chainName: "main"`) and no
/// testnet Stardust exists — `testnet.` and `test.` variants have no DNS. A
/// mainnet server here would scan a chain the voter wallet has no notes on,
/// which surfaces as "no eligible notes" rather than as a misconfiguration.
pub const LIGHTWALLETD_URLS: &[&str] = &["https://testnet.zec.rocks:443"];

/// The Zcash network the staging deployment is anchored to.
///
/// **Testnet**, matching the staging vote chain's own convention. Confirmed
/// three ways rather than assumed: the published `stage/pir.json` snapshot
/// height is above the Zcash mainnet tip and so cannot be a mainnet height;
/// `testnet.zec.rocks` reports `chainName: "test"` with a tip just above that
/// snapshot; and no Stardust host serves testnet.
///
/// Read the *published* config, never a local checkout, when checking this.
/// A stale working copy named a height that was a plausible mainnet height,
/// which is exactly how a wrong network survives review.
pub const ZCASH_NETWORK: zcash_voting::Network = zcash_voting::Network::Testnet;

/// Snapshot height the staging PIR fleet publishes, from `stage/pir.json`.
///
/// Recorded for orientation only; a run should read the live value, because
/// this moves whenever staging re-ingests.
pub const STAGING_PIR_SNAPSHOT_HEIGHT_HINT: u64 = 4_245_460;

/// Tendermint RPC for the staging vote chain.
///
/// `svoted --node` speaks to this. The `vote-chain-primary` host in the
/// dynamic config serves the wallet-facing `shielded-vote/v1` POST routes and
/// a web UI, not the node RPC, so the two are not interchangeable.
pub const STAGING_CHAIN_RPC: &str = "https://stage.vote-rpc-primary.valargroup.org";

/// Wallet-facing vote server, used only as a fallback.
///
/// The live list comes from the published stage config; see
/// [`stage_config`](crate::stage_config). This constant exists so a run can be
/// pointed at one host deliberately, not so endpoints can be hardcoded.
pub const STAGING_VOTE_SERVER_FALLBACK: &str = "https://stage.vote-chain-primary.valargroup.org";

/// Chain id of the staging vote chain, confirmed against its own `/status`.
///
/// `Network::Testnet` already defaults to this, so the default is correct
/// today. [`assert_targets_staging`] still runs before every broadcast: the
/// mapping is a convention rather than a guarantee, a deployment may override
/// the chain id from configuration, and the cost of being wrong is a suite
/// that kills processes mid-broadcast pointed at the production chain.
pub const STAGING_CHAIN_ID: &str = "svote-1";

/// The production vote chain, named only so it can be refused.
const PRODUCTION_CHAIN_ID: &str = "zvote-1";

/// Panics unless `chain_id` is the staging vote chain.
///
/// Called before anything is broadcast. The failure this guards against is not
/// a typo but a silent default: the Zcash network is mainnet, so every
/// convenience path in the SDK that derives a chain id from the network
/// resolves to production.
pub fn assert_targets_staging(chain_id: &str) {
    assert_ne!(
        chain_id, PRODUCTION_CHAIN_ID,
        "refusing to run the crash-recovery suite against the production vote chain"
    );
    assert_eq!(
        chain_id, STAGING_CHAIN_ID,
        "the crash-recovery suite only runs against the staging vote chain"
    );
}

/// Resolved credentials and endpoints for one run.
///
/// Built once, before anything is provisioned or proved. A run that is going
/// to fail for want of a credential should fail in its first second, not forty
/// minutes in with a delegation half-registered on the vote chain.
#[derive(Clone)]
pub struct Environment {
    voter_seed: zeroize::Zeroizing<String>,
    vote_manager_mnemonic: zeroize::Zeroizing<String>,
    chain_rpc: String,
    chain_id: String,
    deployment: crate::stage_config::StageDeployment,
}

/// Why an environment could not be resolved.
#[derive(Debug)]
pub enum EnvironmentError {
    /// The variable was unset or empty.
    Missing { variable: &'static str },
    /// The variable held Infisical's placeholder for a key that does not
    /// exist. `infisical secrets get` exits 0 and substitutes `*not found*`,
    /// so this arrives looking like a value rather than like an error.
    NotFoundPlaceholder { variable: &'static str },
    /// The mnemonic did not have the expected word count.
    UnexpectedWordCount {
        variable: &'static str,
        expected: usize,
        found: usize,
    },
}

impl std::fmt::Display for EnvironmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing { variable } => write!(
                f,
                "{variable} is unset or empty; run under \
                 `infisical run --env={INFISICAL_ENVIRONMENT} -- ...`"
            ),
            Self::NotFoundPlaceholder { variable } => write!(
                f,
                "{variable} holds Infisical's `*not found*` placeholder, so the key does \
                 not exist in the `{INFISICAL_ENVIRONMENT}` environment"
            ),
            Self::UnexpectedWordCount {
                variable,
                expected,
                found,
            } => write!(
                f,
                "{variable} should be a {expected}-word mnemonic but has {found} words; \
                 the three vote credentials have three different shapes and are not \
                 interchangeable"
            ),
        }
    }
}

impl std::error::Error for EnvironmentError {}

/// Infisical's stand-in for a key that does not exist.
const NOT_FOUND_PLACEHOLDER: &str = "*not found*";

const VOTER_SEED_WORDS: usize = 24;
const VOTE_MANAGER_WORDS: usize = 12;

impl Environment {
    /// Resolves every credential and endpoint from the process environment.
    ///
    /// Validates word counts as well as presence. The two mnemonics differ in
    /// length, so a swapped pair is caught here rather than as an
    /// unexplained signature failure against the chain.
    pub fn from_env(
        deployment: crate::stage_config::StageDeployment,
    ) -> Result<Self, EnvironmentError> {
        Ok(Self {
            voter_seed: mnemonic(VOTER_SEED_VAR, VOTER_SEED_WORDS)?,
            vote_manager_mnemonic: mnemonic(VOTE_MANAGER_KEY_VAR, VOTE_MANAGER_WORDS)?,
            chain_rpc: STAGING_CHAIN_RPC.to_string(),
            chain_id: STAGING_CHAIN_ID.to_string(),
            deployment,
        })
    }

    /// The fixed voter's Zcash wallet seed.
    pub fn voter_seed(&self) -> &str {
        &self.voter_seed
    }

    /// The vote manager's cosmos mnemonic.
    pub fn vote_manager_mnemonic(&self) -> &str {
        &self.vote_manager_mnemonic
    }

    pub fn chain_rpc(&self) -> &str {
        &self.chain_rpc
    }

    /// Vote servers from the published stage config, in published order.
    pub fn vote_server_urls(&self) -> Vec<String> {
        self.deployment.vote_server_urls()
    }

    /// PIR endpoints from the published stage config, in published order.
    pub fn pir_urls(&self) -> Vec<String> {
        self.deployment.pir_urls()
    }

    pub fn chain_id(&self) -> &str {
        &self.chain_id
    }
}

/// Neither mnemonic is ever logged, so `Debug` prints only their presence.
impl std::fmt::Debug for Environment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Environment")
            .field("voter_seed", &"<redacted>")
            .field("vote_manager_mnemonic", &"<redacted>")
            .field("chain_rpc", &self.chain_rpc)
            .field("chain_id", &self.chain_id)
            .field("vote_servers", &self.deployment.vote_server_urls())
            .finish()
    }
}

fn mnemonic(
    variable: &'static str,
    expected_words: usize,
) -> Result<zeroize::Zeroizing<String>, EnvironmentError> {
    let raw = std::env::var(variable).unwrap_or_default();
    let value = raw.trim();
    if value.is_empty() {
        return Err(EnvironmentError::Missing { variable });
    }
    if value == NOT_FOUND_PLACEHOLDER {
        return Err(EnvironmentError::NotFoundPlaceholder { variable });
    }
    let found = value.split_whitespace().count();
    if found != expected_words {
        return Err(EnvironmentError::UnexpectedWordCount {
            variable,
            expected: expected_words,
            found,
        });
    }
    Ok(zeroize::Zeroizing::new(value.to_string()))
}
