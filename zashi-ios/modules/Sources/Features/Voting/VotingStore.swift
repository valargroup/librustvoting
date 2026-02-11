import Foundation
import ComposableArchitecture

@Reducer
public struct Voting {
    @ObservableState
    public struct State: Equatable {
        public enum Screen: Equatable {
            case landing
            case keystoneSigning
            case proposalList
            case proposalDetail(id: String)
            case voteReview
            case voteSubmission
            case complete
        }

        public var screenStack: [Screen] = [.landing]
        public var votingRound: VotingRound
        public var votes: [String: VoteChoice] = [:]
        public var votingWeight: UInt64
        public var isKeystoneUser: Bool

        // ZKP #1 (delegation) — runs in background
        public var delegationProofStatus: ProofStatus = .notStarted

        // Submission
        public var submissionStatus: SubmissionStatus = .idle

        public var currentScreen: Screen {
            screenStack.last ?? .landing
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

        public var canSubmitVotes: Bool {
            allVoted && isDelegationReady
        }

        public var nextUnvotedProposalId: String? {
            votingRound.proposals.first { votes[$0.id] == nil }?.id
        }

        public var selectedProposal: Proposal? {
            if case .proposalDetail(let id) = currentScreen {
                return votingRound.proposals.first { $0.id == id }
            }
            return nil
        }

        public init(
            votingRound: VotingRound = MockVotingService.votingRound,
            votingWeight: UInt64 = MockVotingService.votingWeight,
            isKeystoneUser: Bool = false
        ) {
            self.votingRound = votingRound
            self.votingWeight = votingWeight
            self.isKeystoneUser = isKeystoneUser
        }
    }

    public enum Action: Equatable {
        // Navigation
        case dismissFlow
        case goBack

        // Landing
        case beginVotingTapped

        // Keystone signing
        case keystoneApproved
        case keystoneRejected

        // Background ZKP delegation
        case startDelegationProof
        case delegationProofProgress(Double)
        case delegationProofCompleted
        case delegationProofFailed(String)

        // Proposal list
        case proposalTapped(String)

        // Proposal detail
        case castVote(proposalId: String, choice: VoteChoice)
        case backToList

        // Vote review
        case reviewVotesTapped
        case editVote(proposalId: String)
        case submitVotesTapped

        // Vote submission
        case submissionProgress(Int, Int)
        case submissionCompleted
        case submissionFailed(String)

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

            // MARK: - Landing

            case .beginVotingTapped:
                if state.isKeystoneUser {
                    state.screenStack.append(.keystoneSigning)
                } else {
                    state.screenStack.append(.proposalList)
                    return .send(.startDelegationProof)
                }
                return .none

            // MARK: - Keystone Signing

            case .keystoneApproved:
                state.screenStack.removeLast()
                state.screenStack.append(.proposalList)
                return .send(.startDelegationProof)

            case .keystoneRejected:
                state.screenStack.removeLast()
                return .none

            // MARK: - Background ZKP Delegation

            case .startDelegationProof:
                state.delegationProofStatus = .generating(progress: 0)
                return .run { send in
                    // Simulate ZKP generation over ~4 seconds
                    for step in 1...8 {
                        try await Task.sleep(for: .milliseconds(500))
                        await send(.delegationProofProgress(Double(step) / 8.0))
                    }
                    await send(.delegationProofCompleted)
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
                state.screenStack.append(.proposalDetail(id: id))
                return .none

            // MARK: - Proposal Detail

            case .castVote(let proposalId, let choice):
                state.votes[proposalId] = choice
                return .none

            case .backToList:
                if case .proposalDetail = state.currentScreen {
                    state.screenStack.removeLast()
                }
                return .none

            // MARK: - Vote Review

            case .reviewVotesTapped:
                state.screenStack.append(.voteReview)
                return .none

            case .editVote(let proposalId):
                state.screenStack.append(.proposalDetail(id: proposalId))
                return .none

            case .submitVotesTapped:
                state.screenStack.append(.voteSubmission)
                state.submissionStatus = .submitting(proposalIndex: 0, total: state.totalProposals)
                return .run { [total = state.totalProposals] send in
                    for index in 0..<total {
                        await send(.submissionProgress(index, total))
                        // Simulate per-proposal ZKP + submission
                        try await Task.sleep(for: .milliseconds(400))
                    }
                    try await Task.sleep(for: .milliseconds(300))
                    await send(.submissionCompleted)
                }

            // MARK: - Vote Submission

            case .submissionProgress(let index, let total):
                state.submissionStatus = .submitting(proposalIndex: index, total: total)
                return .none

            case .submissionCompleted:
                state.submissionStatus = .complete
                state.screenStack.removeLast()
                state.screenStack.append(.complete)
                return .none

            case .submissionFailed(let error):
                state.submissionStatus = .failed(error)
                return .none

            // MARK: - Complete

            case .doneTapped:
                return .send(.dismissFlow)
            }
        }
    }
}
