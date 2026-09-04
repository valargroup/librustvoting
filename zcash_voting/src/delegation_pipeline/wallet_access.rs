//! On-demand wallet database access for pipeline stages.

use std::borrow::Borrow;

use zcash_protocol::consensus::Parameters;

use crate::backend::zcash_client_sqlite;
use zcash_client_sqlite::WalletDb;

use crate::types::{Network, VotingError};

/// Opens the wallet database for reads on demand.
///
/// Wallet database handles are not `Send`, so the pipeline opens one per
/// stage on the thread that needs it instead of holding one across stages.
pub trait WalletDbOpener: Send + Sync {
    type Conn: Borrow<rusqlite::Connection>;
    type Params: Parameters;
    type Clock;
    type Rng;

    /// Opens a read-capable wallet database handle.
    fn open_for_read(
        &self,
    ) -> Result<WalletDb<Self::Conn, Self::Params, Self::Clock, Self::Rng>, VotingError>;
}

/// Opens a SQLite wallet database by path with the SDK's default settings.
#[derive(Clone, Debug)]
pub struct SqliteWalletDbOpener {
    path: String,
    network: Network,
}

impl SqliteWalletDbOpener {
    pub fn new(path: impl Into<String>, network: Network) -> Self {
        Self {
            path: path.into(),
            network,
        }
    }
}

impl WalletDbOpener for SqliteWalletDbOpener {
    type Conn = rusqlite::Connection;
    type Params = Network;
    type Clock = zcash_client_sqlite::util::SystemClock;
    type Rng = voting_crypto_deps::rand::rngs::OsRng;

    fn open_for_read(
        &self,
    ) -> Result<WalletDb<Self::Conn, Self::Params, Self::Clock, Self::Rng>, VotingError> {
        let conn = rusqlite::Connection::open(&self.path)
            .map_err(|e| VotingError::from_sqlite("failed to open wallet database", &e))?;
        Ok(WalletDb::from_connection(
            conn,
            self.network,
            zcash_client_sqlite::util::SystemClock,
            voting_crypto_deps::rand::rngs::OsRng,
        ))
    }
}
