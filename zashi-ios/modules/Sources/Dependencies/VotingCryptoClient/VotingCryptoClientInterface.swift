import Combine
import ComposableArchitecture
import Foundation
import VotingModels

extension DependencyValues {
    public var votingCrypto: VotingCryptoClient {
        get { self[VotingCryptoClient.self] }
        set { self[VotingCryptoClient.self] = newValue }
    }
}

@DependencyClient
public struct VotingCryptoClient {
    // --- State stream (DB → UI, follows SDKSynchronizer pattern) ---
    public var stateStream: @Sendable () -> AnyPublisher<VotingDbState, Never>
        = { Empty().eraseToAnyPublisher() }

    // --- Database lifecycle ---
    public var openDatabase: @Sendable (_ path: String) async throws -> Void
    public var initRound: @Sendable (_ params: VotingRoundParams, _ sessionJson: String?) async throws -> Void
    public var getRoundState: @Sendable (_ roundId: String) async throws -> RoundStateInfo
    public var getVotes: @Sendable (_ roundId: String) async throws -> [VoteRecord]
    public var listRounds: @Sendable () async throws -> [RoundSummaryInfo]
    public var clearRound: @Sendable (_ roundId: String) async throws -> Void

    // --- Wallet notes ---
    public var getWalletNotes: @Sendable (
        _ walletDbPath: String,
        _ snapshotHeight: UInt64,
        _ networkId: UInt32
    ) async throws -> [NoteInfo]

    // --- Crypto operations ---
    public var generateHotkey: @Sendable (_ roundId: String, _ seed: [UInt8]) async throws -> VotingHotkey
    public var generateDelegationInputs: @Sendable (
        _ senderSeed: [UInt8],
        _ hotkeySeed: [UInt8],
        _ networkId: UInt32,
        _ accountIndex: UInt32
    ) async throws -> DelegationInputs
    public var constructDelegationAction: @Sendable (
        _ roundId: String,
        _ notes: [NoteInfo],
        _ fvkBytes: Data,
        _ gdNewX: Data,
        _ pkdNewX: Data,
        _ hotkeyRawAddress: Data
    ) async throws -> DelegationAction
    public var storeTreeState: @Sendable (_ roundId: String, _ treeState: Data) async throws -> Void
    public var buildDelegationWitness: @Sendable (
        _ roundId: String,
        _ action: DelegationAction,
        _ inclusionProofs: [Data],
        _ exclusionProofs: [Data]
    ) async throws -> Data
    public var generateDelegationProof: @Sendable (_ roundId: String) -> AsyncThrowingStream<ProofEvent, Error>
        = { _ in AsyncThrowingStream { $0.finish() } }
    public var decomposeWeight: @Sendable (_ weight: UInt64) -> [UInt64] = { _ in [] }
    public var encryptShares: @Sendable (
        _ roundId: String,
        _ shares: [UInt64]
    ) async throws -> [EncryptedShare]
    public var buildVoteCommitment: @Sendable (
        _ roundId: String,
        _ proposalId: UInt32,
        _ choice: VoteChoice,
        _ encShares: [EncryptedShare],
        _ vanWitness: Data
    ) -> AsyncThrowingStream<ProofEvent, Error>
        = { _, _, _, _, _ in AsyncThrowingStream { $0.finish() } }
    public var buildSharePayloads: @Sendable (
        _ encShares: [EncryptedShare],
        _ commitment: VoteCommitmentBundle
    ) async throws -> [SharePayload]
    public var markVoteSubmitted: @Sendable (_ roundId: String, _ proposalId: UInt32) async throws -> Void
}
