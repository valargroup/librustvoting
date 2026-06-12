use orchard::builder::BundleProtocol;
use orchard::note::NoteVersion;
use zcash_protocol::consensus::{BlockHeight, BranchId, Parameters};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum VotingShieldedProtocol {
    Orchard,
    #[cfg(zcash_unstable = "nu7")]
    Ironwood,
}

impl VotingShieldedProtocol {
    pub(crate) fn for_branch_id(_branch_id: BranchId) -> Self {
        #[cfg(zcash_unstable = "nu7")]
        if matches!(_branch_id, BranchId::Nu7) {
            return Self::Ironwood;
        }

        Self::Orchard
    }

    pub(crate) fn for_height<P: Parameters>(params: &P, height: BlockHeight) -> Self {
        Self::for_branch_id(BranchId::for_height(params, height))
    }

    pub(crate) fn bundle_protocol(self) -> BundleProtocol {
        match self {
            Self::Orchard => BundleProtocol::Orchard,
            #[cfg(zcash_unstable = "nu7")]
            Self::Ironwood => BundleProtocol::Ironwood,
        }
    }

    pub(crate) fn note_version(self) -> NoteVersion {
        match self {
            Self::Orchard => NoteVersion::V2,
            #[cfg(zcash_unstable = "nu7")]
            Self::Ironwood => NoteVersion::V3,
        }
    }

    pub(crate) fn pool(self) -> &'static str {
        match self {
            Self::Orchard => "orchard",
            #[cfg(zcash_unstable = "nu7")]
            Self::Ironwood => "ironwood",
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Orchard => "Orchard",
            #[cfg(zcash_unstable = "nu7")]
            Self::Ironwood => "Ironwood",
        }
    }
}
