import SwiftUI
import ComposableArchitecture
import Generated
import UIComponents

struct ProposalListView: View {
    @Environment(\.colorScheme) var colorScheme

    let store: StoreOf<Voting>

    var body: some View {
        WithPerceptionTracking {
            VStack(spacing: 0) {
                // ZKP status banner
                if store.delegationProofStatus != .notStarted && store.delegationProofStatus != .complete {
                    ZKPStatusBanner(proofStatus: store.delegationProofStatus)
                        .padding(.horizontal, 24)
                        .padding(.top, 8)
                        .transition(.move(edge: .top).combined(with: .opacity))
                }

                // Progress indicator
                HStack {
                    Text("\(store.votedCount) of \(store.totalProposals) voted")
                        .zFont(.medium, size: 14, style: Design.Text.secondary)

                    Spacer()

                    if store.isDelegationReady {
                        HStack(spacing: 4) {
                            Image(systemName: "checkmark.circle.fill")
                                .foregroundStyle(.green)
                                .font(.system(size: 12))
                            Text("Delegation ready")
                                .font(.system(size: 12, weight: .medium))
                                .foregroundStyle(.green)
                        }
                    }
                }
                .padding(.horizontal, 24)
                .padding(.top, 16)
                .padding(.bottom, 8)

                // Proposal cards
                ScrollViewReader { proxy in
                    ScrollView {
                        LazyVStack(spacing: 12) {
                            ForEach(store.votingRound.proposals) { proposal in
                                proposalCard(proposal, scrollProxy: proxy)
                                    .id(proposal.id)
                            }
                        }
                        .padding(.horizontal, 24)
                        .padding(.bottom, 100)
                    }
                }

                // Floating review button
                VStack {
                    ZashiButton(
                        "Review & Submit",
                        type: store.canSubmitVotes ? .primary : .quaternary
                    ) {
                        store.send(.reviewVotesTapped)
                    }
                    .disabled(!store.canSubmitVotes)
                    .padding(.horizontal, 24)
                    .padding(.vertical, 12)
                    .background {
                        LinearGradient(
                            colors: [
                                Design.Surfaces.bgPrimary.color(colorScheme).opacity(0),
                                Design.Surfaces.bgPrimary.color(colorScheme),
                            ],
                            startPoint: .top,
                            endPoint: UnitPoint(x: 0.5, y: 0.3)
                        )
                    }
                }
            }
            .navigationTitle(store.votingRound.title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .navigationBarLeading) {
                    Button {
                        store.send(.goBack)
                    } label: {
                        Image(systemName: "chevron.left")
                    }
                }
            }
        }
    }

    @ViewBuilder
    private func proposalCard(_ proposal: Proposal, scrollProxy proxy: ScrollViewProxy) -> some View {
        let vote = store.votes[proposal.id]

        VStack(alignment: .leading, spacing: 10) {
            // Header: title + chip (if voted)
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 4) {
                    if let zip = proposal.zipNumber {
                        ZIPBadge(zipNumber: zip)
                    }
                    Text(proposal.title)
                        .zFont(.semiBold, size: 16, style: Design.Text.primary)
                }

                Spacer(minLength: 8)

                if vote != nil {
                    VoteChip(choice: vote)
                }

                // Detail chevron
                Button {
                    store.send(.proposalTapped(proposal.id))
                } label: {
                    Image(systemName: "chevron.right")
                        .font(.system(size: 12, weight: .semibold))
                        .foregroundStyle(Design.Text.tertiary.color(colorScheme))
                        .frame(width: 24, height: 24)
                }
            }

            Text(proposal.description)
                .zFont(.regular, size: 13, style: Design.Text.secondary)
                .lineLimit(2)

            // Inline vote buttons (show when not yet voted)
            if vote == nil {
                HStack(spacing: 8) {
                    inlineVoteButton("Support", color: .green, icon: "hand.thumbsup") {
                        castAndScroll(proposalId: proposal.id, choice: .support, proxy: proxy)
                    }
                    inlineVoteButton("Oppose", color: .red, icon: "hand.thumbsdown") {
                        castAndScroll(proposalId: proposal.id, choice: .oppose, proxy: proxy)
                    }
                    inlineVoteButton("Skip", color: .gray, icon: "forward") {
                        castAndScroll(proposalId: proposal.id, choice: .skip, proxy: proxy)
                    }
                }
                .padding(.top, 4)
            }
        }
        .padding(16)
        .background(Design.Surfaces.bgPrimary.color(colorScheme))
        .clipShape(RoundedRectangle(cornerRadius: 16))
        .overlay(
            RoundedRectangle(cornerRadius: 16)
                .stroke(
                    vote != nil ? voteColor(vote).opacity(0.3) : Design.Surfaces.strokeSecondary.color(colorScheme),
                    lineWidth: 1
                )
        )
        .shadow(color: .black.opacity(0.04), radius: 2, x: 0, y: 1)
        .animation(.easeInOut(duration: 0.2), value: vote)
    }

    @ViewBuilder
    private func inlineVoteButton(
        _ title: String,
        color: Color,
        icon: String,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            HStack(spacing: 4) {
                Image(systemName: icon)
                    .font(.system(size: 11))
                Text(title)
                    .font(.system(size: 13, weight: .semibold))
            }
            .foregroundStyle(color)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 10)
            .background(color.opacity(0.1))
            .clipShape(RoundedRectangle(cornerRadius: 10))
        }
    }

    private func castAndScroll(proposalId: String, choice: VoteChoice, proxy: ScrollViewProxy) {
        store.send(.castVote(proposalId: proposalId, choice: choice))

        // Find the next unvoted proposal after this one
        let proposals = store.votingRound.proposals
        if let currentIndex = proposals.firstIndex(where: { $0.id == proposalId }) {
            // Look for the next unvoted proposal after the current one
            let nextUnvoted = proposals[(currentIndex + 1)...].first { store.votes[$0.id] == nil }
                // Fall back to any unvoted proposal before the current one
                ?? proposals[..<currentIndex].first { store.votes[$0.id] == nil }

            if let target = nextUnvoted {
                withAnimation {
                    proxy.scrollTo(target.id, anchor: .top)
                }
            }
        }
    }

    private func voteColor(_ vote: VoteChoice?) -> Color {
        guard let vote else { return .clear }
        switch vote {
        case .support: return .green
        case .oppose: return .red
        case .skip: return .gray
        }
    }
}
