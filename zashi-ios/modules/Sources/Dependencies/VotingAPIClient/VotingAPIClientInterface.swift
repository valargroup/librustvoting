import ComposableArchitecture
import Foundation
import VotingModels

extension DependencyValues {
    public var votingAPI: VotingAPIClient {
        get { self[VotingAPIClient.self] }
        set { self[VotingAPIClient.self] = newValue }
    }
}

@DependencyClient
public struct VotingAPIClient {
    public var fetchActiveVotingSession: @Sendable () async throws -> VotingSession
    public var fetchVotingWeight: @Sendable (_ snapshotHeight: UInt64) async throws -> UInt64
    public var fetchNoteInclusionProofs: @Sendable (_ commitments: [Data]) async throws -> [Data]
    public var fetchNullifierExclusionProofs: @Sendable (_ nullifiers: [Data]) async throws -> [Data]
    public var fetchCommitmentTreeState: @Sendable (_ height: UInt64) async throws -> CommitmentTreeState
    public var fetchLatestCommitmentTree: @Sendable () async throws -> CommitmentTreeState
    public var submitDelegation: @Sendable (_ registration: DelegationRegistration) async throws -> TxResult
    public var submitVoteCommitment: @Sendable (_ bundle: VoteCommitmentBundle, _ signature: CastVoteSignature) async throws -> TxResult
    public var delegateShares: @Sendable (_ payloads: [SharePayload], _ roundIdHex: String) async throws -> Void
    public var fetchProposalTally: @Sendable (_ roundId: Data, _ proposalId: UInt32) async throws -> TallyResult
    public var awaitCommitmentTreeGrowth: @Sendable (_ previousNextIndex: UInt64, _ timeoutSeconds: TimeInterval) async throws -> CommitmentTreeState
    /// Create a new voting session on chain (dev/test only).
    public var createTestSession: @Sendable (_ payload: [String: Any]) async throws -> Void
}
