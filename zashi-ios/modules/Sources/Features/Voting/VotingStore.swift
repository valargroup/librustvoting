import Foundation
import ComposableArchitecture
import VotingAPIClient
import VotingCryptoClient
import VotingModels

@Reducer
public struct Voting {
    @Dependency(\.votingAPI) var votingAPI
    @Dependency(\.votingCrypto) var votingCrypto
    @ObservableState
    public struct State: Equatable {
        public enum Screen: Equatable {
            case delegationSigning
            case proposalList
            case proposalDetail(id: UInt32)
            case complete
        }

        public struct PendingVote: Equatable {
            public var proposalId: UInt32
            public var choice: VoteChoice
        }

        public var screenStack: [Screen] = [.delegationSigning]
        public var votingRound: VotingRound
        public var votes: [UInt32: VoteChoice] = [:]
        public var votingWeight: UInt64
        public var isKeystoneUser: Bool
        public var roundId: String

        public var selectedProposalId: UInt32?

        // Vote awaiting user confirmation in detail view
        public var pendingVote: PendingVote?

        // ZKP #1 (delegation) — runs in background
        public var delegationProofStatus: ProofStatus = .notStarted

        public var currentScreen: Screen {
            screenStack.last ?? .proposalList
        }

        public var votingWeightZECString: String {
            let zec = Double(votingWeight) / 100_000_000.0
            return String(format: "%.2f", zec)
        }

        public var votedCount: Int {
            votes.count
        }

        public var totalProposals: Int {
            votingRound.proposals.count
        }

        public var allVoted: Bool {
            votedCount == totalProposals
        }

        public var isDelegationReady: Bool {
            delegationProofStatus == .complete
        }

        /// Whether the previous vote's VAN has landed in the vote commitment tree.
        /// Always true in the prototype; real implementation checks tree sync.
        public var canConfirmVote: Bool {
            true
        }

        public var nextUnvotedProposalId: UInt32? {
            votingRound.proposals.first { votes[$0.id] == nil }?.id
        }

        public var activeProposalId: UInt32? {
            selectedProposalId ?? nextUnvotedProposalId
        }

        public var selectedProposal: Proposal? {
            if case .proposalDetail(let id) = currentScreen {
                return votingRound.proposals.first { $0.id == id }
            }
            return nil
        }

        // Index of the proposal currently shown in detail
        public var detailProposalIndex: Int? {
            if case .proposalDetail(let id) = currentScreen {
                return votingRound.proposals.firstIndex { $0.id == id }
            }
            return nil
        }

        public init(
            votingRound: VotingRound = MockVotingService.votingRound,
            votingWeight: UInt64 = MockVotingService.votingWeight,
            isKeystoneUser: Bool = false,
            roundId: String = "01010101010101010101010101010101"
        ) {
            self.votingRound = votingRound
            self.votingWeight = votingWeight
            self.isKeystoneUser = isKeystoneUser
            self.roundId = roundId
        }
    }

    public enum Action: Equatable {
        // Navigation
        case dismissFlow
        case goBack

        // Delegation signing
        case delegationApproved
        case delegationRejected

        // Background ZKP delegation
        case startDelegationProof
        case delegationProofProgress(Double)
        case delegationProofCompleted
        case delegationProofFailed(String)

        // Proposal list
        case proposalTapped(UInt32)

        // Proposal detail
        case castVote(proposalId: UInt32, choice: VoteChoice)
        case confirmVote
        case cancelPendingVote
        case advanceAfterVote(nextId: UInt32?)
        case backToList
        case nextProposalDetail
        case previousProposalDetail

        // Complete
        case doneTapped
    }

    public init() {}

    public var body: some Reducer<State, Action> {
        Reduce { state, action in
            switch action {
            // MARK: - Navigation

            case .dismissFlow:
                return .none

            case .goBack:
                if state.screenStack.count > 1 {
                    state.screenStack.removeLast()
                }
                return .none

            // MARK: - Delegation Signing

            case .delegationApproved:
                state.screenStack = [.proposalList]
                return .send(.startDelegationProof)

            case .delegationRejected:
                return .send(.dismissFlow)

            // MARK: - Background ZKP Delegation

            case .startDelegationProof:
                state.delegationProofStatus = .generating(progress: 0)
                let roundId = state.roundId
                let snapshotHeight = state.votingRound.snapshotHeight
                return .run { [votingCrypto] send in
                    // Open database
                    let dbPath = FileManager.default
                        .urls(for: .documentDirectory, in: .userDomainMask)[0]
                        .appendingPathComponent("voting.sqlite3").path
                    try await votingCrypto.openDatabase(dbPath)

                    // Clear any previous data for this round, then initialize
                    try? await votingCrypto.clearRound(roundId)
                    let params = VotingRoundParams(
                        voteRoundId: Data(repeating: 0x01, count: 16),
                        snapshotHeight: snapshotHeight,
                        eaPK: Data(repeating: 0xEA, count: 32),
                        ncRoot: Data(repeating: 0xAA, count: 32),
                        nullifierIMTRoot: Data(repeating: 0xBB, count: 32)
                    )
                    try await votingCrypto.initRound(params, nil)

                    // Stub delegation setup: hotkey → action → witness
                    let hotkey = try await votingCrypto.generateHotkey(roundId)
                    let mockNote = NoteInfo(
                        commitment: Data(repeating: 0x01, count: 32),
                        nullifier: Data(repeating: 0x02, count: 32),
                        value: 1_000_000,
                        position: 42
                    )
                    let action = try await votingCrypto.constructDelegationAction(
                        roundId, hotkey, [mockNote]
                    )
                    _ = try await votingCrypto.buildDelegationWitness(
                        roundId, action,
                        [Data(repeating: 0x11, count: 32)],
                        [Data(repeating: 0x22, count: 32)]
                    )

                    // Generate delegation proof (long-running, reports progress)
                    for try await event in votingCrypto.generateDelegationProof(roundId) {
                        switch event {
                        case .progress(let p):
                            await send(.delegationProofProgress(p))
                        case .completed:
                            await send(.delegationProofCompleted)
                        }
                    }
                } catch: { error, send in
                    await send(.delegationProofFailed(error.localizedDescription))
                }

            case .delegationProofProgress(let progress):
                state.delegationProofStatus = .generating(progress: progress)
                return .none

            case .delegationProofCompleted:
                state.delegationProofStatus = .complete
                return .none

            case .delegationProofFailed(let error):
                state.delegationProofStatus = .failed(error)
                return .none

            // MARK: - Proposal List

            case .proposalTapped(let id):
                state.selectedProposalId = id
                state.screenStack.append(.proposalDetail(id: id))
                return .none

            // MARK: - Proposal Detail

            case .castVote(let proposalId, let choice):
                // If already confirmed for this proposal, ignore
                guard state.votes[proposalId] == nil else { return .none }
                state.pendingVote = .init(proposalId: proposalId, choice: choice)
                return .none

            case .cancelPendingVote:
                state.pendingVote = nil
                return .none

            case .confirmVote:
                guard let pending = state.pendingVote else { return .none }
                state.votes[pending.proposalId] = pending.choice
                state.pendingVote = nil

                let proposalId = pending.proposalId
                let choice = pending.choice
                let roundId = state.roundId
                let votingWeight = state.votingWeight
                let nextId = nextUnvotedId(after: proposalId, in: state)

                return .merge(
                    // Submit this vote to chain in background
                    .run { [votingAPI, votingCrypto] send in
                        // Decompose weight into binary shares, encrypt under EA public key
                        let shares = votingCrypto.decomposeWeight(votingWeight)
                        print("[Voting] decomposeWeight(\(votingWeight)) → \(shares.count) shares")
                        let encShares = try await votingCrypto.encryptShares(roundId, shares)

                        let vanWitness = Data(repeating: 0xDD, count: 64) // mock VAN witness

                        // Build vote commitment + ZKP #2 (stored in DB)
                        var proofData = Data()
                        for try await event in votingCrypto.buildVoteCommitment(roundId, proposalId, choice, encShares, vanWitness) {
                            if case .completed(let proof) = event {
                                proofData = proof
                            }
                        }

                        // Submit to chain (API stubs for now)
                        let bundle = VoteCommitmentBundle(
                            vanNullifier: Data(repeating: 0, count: 32),
                            voteAuthorityNoteNew: Data(repeating: 0, count: 32),
                            voteCommitment: Data(repeating: 0, count: 32),
                            proposalId: proposalId,
                            proof: proofData,
                            voteRoundId: Data(repeating: 0, count: 32),
                            voteCommTreeAnchorHeight: 0
                        )
                        _ = try await votingAPI.submitVoteCommitment(bundle)
                        let payloads = try await votingCrypto.buildSharePayloads(encShares, bundle)
                        try await votingAPI.delegateShares(payloads)

                        // Mark vote submitted in DB
                        try await votingCrypto.markVoteSubmitted(roundId, proposalId)
                    } catch: { error, _ in
                        print("[Voting] vote submission failed: \(error)")
                    },
                    // Advance UI after brief pause
                    .run { send in
                        try await Task.sleep(for: .milliseconds(600))
                        await send(.advanceAfterVote(nextId: nextId))
                    }
                )

            case .advanceAfterVote(let nextId):
                if case .proposalDetail = state.currentScreen {
                    if let nextId {
                        state.selectedProposalId = nextId
                        state.screenStack.removeLast()
                        state.screenStack.append(.proposalDetail(id: nextId))
                    } else {
                        // All proposals voted — go to completion
                        state.screenStack = [.complete]
                    }
                }
                return .none

            case .backToList:
                state.pendingVote = nil
                if case .proposalDetail = state.currentScreen {
                    state.screenStack.removeLast()
                }
                return .none

            case .nextProposalDetail:
                state.pendingVote = nil
                if let index = state.detailProposalIndex,
                   index + 1 < state.votingRound.proposals.count {
                    let nextId = state.votingRound.proposals[index + 1].id
                    state.selectedProposalId = nextId
                    state.screenStack.removeLast()
                    state.screenStack.append(.proposalDetail(id: nextId))
                }
                return .none

            case .previousProposalDetail:
                state.pendingVote = nil
                if let index = state.detailProposalIndex, index > 0 {
                    let prevId = state.votingRound.proposals[index - 1].id
                    state.selectedProposalId = prevId
                    state.screenStack.removeLast()
                    state.screenStack.append(.proposalDetail(id: prevId))
                }
                return .none

            // MARK: - Complete

            case .doneTapped:
                return .send(.dismissFlow)
            }
        }
    }

    // Find the next unvoted proposal after the given one (wrapping around)
    private func nextUnvotedId(after proposalId: UInt32, in state: State) -> UInt32? {
        let proposals = state.votingRound.proposals
        guard let currentIndex = proposals.firstIndex(where: { $0.id == proposalId }) else { return nil }

        // Look forward first, then wrap
        return proposals[(currentIndex + 1)...].first { state.votes[$0.id] == nil }?.id
            ?? proposals[..<currentIndex].first { state.votes[$0.id] == nil }?.id
    }
}
