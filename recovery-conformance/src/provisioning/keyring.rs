//! A throwaway `svoted` keyring holding the vote manager for one run.
//!
//! `svoted` signs from a keyring, and the backend chosen for this suite is
//! `test`, which stores the key **unencrypted**. That is a deliberate, narrow
//! exception and it is contained here rather than spread through the caller:
//!
//! - the keyring lives in a fresh `0700` directory outside the repository,
//!   never the operator's real `~/.svoted`;
//! - the mnemonic reaches `svoted` through a `0600` file that is deleted the
//!   moment the import returns, never through `argv`, which any local process
//!   can read out of `ps`;
//! - the directory is removed when the handle drops, including on unwind.
//!
//! Even so, the key is briefly at rest outside Infisical and the system
//! keychain. Treat `VOTE_MANAGER_VOTE_SDK` as rotate-on-suspicion.
//!
//! [`VoteManagerKeyring::derive_address`] exists so the common case — checking
//! we hold the right key — never persists anything at all.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Keyring backend. `test` stores keys unencrypted; see the module docs.
const KEYRING_BACKEND: &str = "test";

/// Coin type this deployment's accounts are derived under.
///
/// **133, Zcash's, not the cosmos default of 118.** The coordinator keys were
/// created in a browser wallet against a chain registration that specifies 133,
/// so deriving with `svoted`'s own default produces a valid-looking `sv1...`
/// address that is simply a different account — one the chain has never heard
/// of. Verified: `m/44'/133'/0'/0/0` reproduces the registered coordinator,
/// while 118, 60 and 1 all do not.
const DEFAULT_COIN_TYPE: u32 = 133;

/// Key name inside the throwaway keyring. Local to it, so it need not be
/// unique beyond this handle.
const KEY_NAME: &str = "recovery-conformance-vote-manager";

/// A temporary `svoted` home containing exactly one imported key.
///
/// Dropping it removes the directory and the key inside.
pub struct VoteManagerKeyring {
    home: PathBuf,
    address: String,
}

impl VoteManagerKeyring {
    /// Derives the account address for `mnemonic` **without storing it**.
    ///
    /// Uses `--dry-run`, so nothing is written to any keyring. This is how a
    /// run checks it holds the right key before it broadcasts anything: the
    /// address can be compared against the vote manager the stage static
    /// config names, and a wrong key fails immediately rather than as a
    /// rejected transaction.
    pub fn derive_address(mnemonic: &str) -> Result<String> {
        Self::derive_address_at(mnemonic, 0, 0)
    }

    /// Derives the address at an explicit BIP44 account and index, storing
    /// nothing.
    ///
    /// The suite signs from the default `0/0`. This exists because a key
    /// derived by a browser wallet may sit at another index, and "wrong
    /// derivation path" and "different key entirely" are otherwise
    /// indistinguishable from a single mismatched address.
    pub fn derive_address_at(mnemonic: &str, account: u32, index: u32) -> Result<String> {
        Self::derive_address_with(mnemonic, DEFAULT_COIN_TYPE, account, index)
    }

    /// Derives at an explicit coin type as well, storing nothing.
    ///
    /// A browser wallet may register a chain with a coin type other than the
    /// cosmos default, in which case the same mnemonic yields a different
    /// account than `svoted` produces from its own defaults. Without this the
    /// only visible symptom is an address that is simply "wrong".
    pub fn derive_address_with(
        mnemonic: &str,
        coin_type: u32,
        account: u32,
        index: u32,
    ) -> Result<String> {
        let coin_type = coin_type.to_string();
        let account = account.to_string();
        let index = index.to_string();
        let scratch = TempDir::new()?;
        let source = scratch.write_mnemonic(mnemonic)?;
        let output = svoted(&[
            "keys",
            "add",
            KEY_NAME,
            "--recover",
            "--dry-run",
            "--source",
            source.to_str().context("mnemonic path is not UTF-8")?,
            "--keyring-backend",
            KEYRING_BACKEND,
            "--home",
            scratch.path().to_str().context("home path is not UTF-8")?,
            "--coin-type",
            &coin_type,
            "--account",
            &account,
            "--index",
            &index,
            "--output",
            "json",
        ])?;
        address_from(&output)
    }

    /// Imports `mnemonic` into a fresh throwaway keyring.
    pub fn import(mnemonic: &str) -> Result<Self> {
        // Held as a `TempDir` for the whole import, so any early return below
        // removes the directory and the key inside it. Only a fully successful
        // import promotes it into a keyring the caller owns.
        let scratch = TempDir::new()?;
        let source = scratch.write_mnemonic(mnemonic)?;
        let output = svoted(&[
            "keys",
            "add",
            KEY_NAME,
            "--recover",
            "--source",
            source.to_str().context("mnemonic path is not UTF-8")?,
            "--keyring-backend",
            KEYRING_BACKEND,
            "--home",
            scratch.path().to_str().context("home path is not UTF-8")?,
            "--output",
            "json",
        ]);
        // Remove the mnemonic file before inspecting the result, so a failed
        // import does not leave it behind.
        let _ = std::fs::remove_file(&source);
        let address = address_from(&output?)?;
        Ok(Self {
            home: scratch.keep(),
            address,
        })
    }

    /// The imported account's bech32 address.
    pub fn address(&self) -> &str {
        &self.address
    }

    /// Flags every signing invocation must carry to reach this keyring.
    pub fn signing_flags(&self) -> Vec<String> {
        vec![
            "--keyring-backend".to_string(),
            KEYRING_BACKEND.to_string(),
            "--home".to_string(),
            self.home.to_string_lossy().into_owned(),
            "--from".to_string(),
            KEY_NAME.to_string(),
        ]
    }
}

impl Drop for VoteManagerKeyring {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

/// A `0700` directory removed on drop unless explicitly kept.
struct TempDir {
    path: Option<PathBuf>,
}

impl TempDir {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "recovery-conformance-keyring-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&path).with_context(|| format!("creating {}", path.display()))?;
        restrict(&path, 0o700)?;
        Ok(Self { path: Some(path) })
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("temp dir is live")
    }

    fn write_mnemonic(&self, mnemonic: &str) -> Result<PathBuf> {
        write_mnemonic(self.path(), mnemonic)
    }

    /// Hands ownership of the directory to the caller.
    fn keep(mut self) -> PathBuf {
        self.path.take().expect("temp dir is live")
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Writes the mnemonic to a `0600` file for `svoted --source`.
///
/// A file rather than `argv`: command lines are visible to every local process
/// through `ps`, and a mnemonic passed that way would be readable by anything
/// on the machine for the lifetime of the call.
fn write_mnemonic(directory: &Path, mnemonic: &str) -> Result<PathBuf> {
    let path = directory.join("mnemonic");
    std::fs::write(&path, mnemonic.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    restrict(&path, 0o600)?;
    Ok(path)
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("restricting {}", path.display()))
}

/// Runs `svoted` and returns stdout.
///
/// Errors carry stderr, never the arguments: an argument list for a key
/// command can name paths that held the mnemonic.
fn svoted(args: &[&str]) -> Result<String> {
    let output = Command::new("svoted")
        .args(args)
        .output()
        .context("running svoted; is it on PATH?")?;
    if !output.status.success() {
        bail!(
            "svoted {} failed: {}",
            args.first().copied().unwrap_or("?"),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Extracts the bech32 address from a `keys add --output json` response.
fn address_from(output: &str) -> Result<String> {
    let parsed: serde_json::Value =
        serde_json::from_str(output.trim()).context("svoted keys add did not return JSON")?;
    parsed
        .get("address")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .context("svoted keys add returned no address")
}
