import Foundation
import VotingModels

public enum MockVotingService {
    public static let votingRound = VotingRound(
        id: "nu7-sentiment-0xab3f7c91e2d4",
        title: "NU7 Sentiment Poll",
        description: "Community sentiment polling for proposed NU7 network upgrade features. Your shielded balance is used as voting weight.",
        snapshotHeight: 3_235_470,
        snapshotDate: Calendar.current.date(byAdding: .day, value: -3, to: Date())!,
        votingStart: Calendar.current.date(byAdding: .day, value: -1, to: Date())!,
        votingEnd: Calendar.current.date(byAdding: .day, value: 5, to: Date())!,
        proposals: proposals
    )

    public static let proposals: [Proposal] = [
        Proposal(id: 1, title: "Zcash Shielded Assets (ZSAs)", description: "Enable custom tokens on the Zcash network via shielded asset issuance and transfer, expanding Zcash beyond ZEC while preserving privacy.", zipNumber: "ZIP-227", forumURL: URL(string: "https://forum.zcashcommunity.com/t/zsa")),
        Proposal(id: 2, title: "Network Sustainability Mechanism (NSM)", description: "Introduce a smoothed, market-based issuance mechanism to ensure long-term sustainability of network security incentives.", zipNumber: "ZIP-234", forumURL: URL(string: "https://forum.zcashcommunity.com/t/nsm")),
        Proposal(id: 3, title: "Burning 60% of transaction fees", description: "Burn 60% of all transaction fees to create deflationary pressure and align miner incentives with network health.", forumURL: URL(string: "https://forum.zcashcommunity.com/t/burn-fees")),
        Proposal(id: 4, title: "Memo Bundles", description: "Extend the memo system to support structured, multi-part memo bundles for richer application-layer protocols.", forumURL: URL(string: "https://forum.zcashcommunity.com/t/memo-bundles")),
        Proposal(id: 5, title: "Explicit Fees", description: "Require transactions to explicitly declare fees rather than inferring them, improving transparency and wallet UX.", forumURL: URL(string: "https://forum.zcashcommunity.com/t/explicit-fees")),
        Proposal(id: 6, title: "Disallowing v4 transactions", description: "Remove support for legacy v4 transparent transactions to simplify the protocol and encourage shielded usage.", forumURL: URL(string: "https://forum.zcashcommunity.com/t/disallow-v4")),
        Proposal(id: 7, title: "Project Tachyon", description: "A research initiative to dramatically improve Zcash sync performance through novel cryptographic techniques.", forumURL: URL(string: "https://forum.zcashcommunity.com/t/tachyon")),
        Proposal(id: 8, title: "STARK proof verification via TZEs", description: "Enable STARK proof verification through Time-locked Zero-knowledge Extensions, expanding Zcash's programmability.", forumURL: URL(string: "https://forum.zcashcommunity.com/t/stark-tze")),
        Proposal(id: 9, title: "Dynamic fee mechanism", description: "Implement a dynamic fee algorithm that adjusts fees based on network demand, improving congestion management.", forumURL: URL(string: "https://forum.zcashcommunity.com/t/dynamic-fees")),
        Proposal(id: 10, title: "Consensus accounts", description: "Introduce consensus-level account abstractions to support more complex on-chain logic and state management.", forumURL: URL(string: "https://forum.zcashcommunity.com/t/consensus-accounts")),
        Proposal(id: 11, title: "Orchard quantum recoverability", description: "Add quantum-resistant key recovery mechanisms to the Orchard shielded pool, future-proofing funds against quantum attacks.", forumURL: URL(string: "https://forum.zcashcommunity.com/t/quantum-recovery")),
    ]
}
