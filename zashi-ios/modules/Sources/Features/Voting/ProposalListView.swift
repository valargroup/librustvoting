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
                                proposalCard(proposal)
                                    .id(proposal.id)
                            }
                        }
                        .padding(.horizontal, 24)
                        .padding(.bottom, 24)
                    }

                    bottomBar(scrollProxy: proxy)
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

    // MARK: - Bottom Bar

    @ViewBuilder
    private func bottomBar(scrollProxy proxy: ScrollViewProxy) -> some View {
        let nextUnvoted = store.nextUnvotedProposalId
            .flatMap { id in store.votingRound.proposals.first { $0.id == id } }

        VStack(spacing: 0) {
            Divider()

            if let proposal = nextUnvoted {
                VStack(spacing: 10) {
                    HStack {
                        if let zip = proposal.zipNumber {
                            Text(zip)
                                .font(.system(size: 11, weight: .medium, design: .monospaced))
                                .foregroundStyle(.secondary)
                        }
                        Text(proposal.title)
                            .zFont(.medium, size: 13, style: Design.Text.primary)
                            .lineLimit(1)
                        Spacer()
                    }

                    HStack(spacing: 8) {
                        bottomVoteButton("Support", color: .green, icon: "hand.thumbsup") {
                            vote(proposalId: proposal.id, choice: .support, proxy: proxy)
                        }
                        bottomVoteButton("Oppose", color: .red, icon: "hand.thumbsdown") {
                            vote(proposalId: proposal.id, choice: .oppose, proxy: proxy)
                        }
                        bottomVoteButton("Skip", color: .gray, icon: "forward") {
                            vote(proposalId: proposal.id, choice: .skip, proxy: proxy)
                        }
                    }
                }
                .padding(.horizontal, 24)
                .padding(.vertical, 12)
            } else {
                // All voted
                VStack(spacing: 8) {
                    Text("All proposals voted!")
                        .zFont(.medium, size: 13, style: Design.Text.secondary)

                    ZashiButton(
                        "Review & Submit",
                        type: store.canSubmitVotes ? .primary : .quaternary
                    ) {
                        store.send(.reviewVotesTapped)
                    }
                    .disabled(!store.canSubmitVotes)
                }
                .padding(.horizontal, 24)
                .padding(.vertical, 12)
            }
        }
        .background(Design.Surfaces.bgPrimary.color(colorScheme))
    }

    // MARK: - Card

    @ViewBuilder
    private func proposalCard(_ proposal: Proposal) -> some View {
        let vote = store.votes[proposal.id]
        let isNext = store.nextUnvotedProposalId == proposal.id

        VStack(alignment: .leading, spacing: 10) {
            if isNext {
                Text("UP NEXT")
                    .font(.system(size: 10, weight: .bold, design: .rounded))
                    .foregroundStyle(Design.Surfaces.brandPrimary.color(colorScheme))
                    .padding(.bottom, -4)
            }

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
        }
        .padding(16)
        .background(
            isNext
                ? Design.Surfaces.brandPrimary.color(colorScheme).opacity(0.04)
                : Design.Surfaces.bgPrimary.color(colorScheme)
        )
        .clipShape(RoundedRectangle(cornerRadius: 16))
        .overlay(
            RoundedRectangle(cornerRadius: 16)
                .stroke(
                    isNext
                        ? Design.Surfaces.brandPrimary.color(colorScheme)
                        : vote != nil
                            ? voteColor(vote).opacity(0.3)
                            : Design.Surfaces.strokeSecondary.color(colorScheme),
                    lineWidth: isNext ? 2 : 1
                )
        )
        .shadow(color: .black.opacity(0.04), radius: 2, x: 0, y: 1)
        .animation(.easeInOut(duration: 0.2), value: vote)
        .animation(.easeInOut(duration: 0.2), value: isNext)
    }

    // MARK: - Components

    @ViewBuilder
    private func bottomVoteButton(
        _ title: String,
        color: Color,
        icon: String,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            HStack(spacing: 4) {
                Image(systemName: icon)
                    .font(.system(size: 12))
                Text(title)
                    .font(.system(size: 14, weight: .semibold))
            }
            .foregroundStyle(color)
            .frame(maxWidth: .infinity)
            .padding(.vertical, 12)
            .background(color.opacity(0.1))
            .clipShape(RoundedRectangle(cornerRadius: 12))
        }
    }

    private func vote(proposalId: String, choice: VoteChoice, proxy: ScrollViewProxy) {
        store.send(.castVote(proposalId: proposalId, choice: choice))

        // Scroll to the next unvoted proposal (computed after this vote is applied)
        // We need to find it ourselves since the store hasn't updated yet in this same frame
        let proposals = store.votingRound.proposals
        if let currentIndex = proposals.firstIndex(where: { $0.id == proposalId }) {
            let nextUnvoted = proposals[(currentIndex + 1)...].first { store.votes[$0.id] == nil }
                ?? proposals[..<currentIndex].first { store.votes[$0.id] == nil }

            if let target = nextUnvoted {
                DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) {
                    withAnimation(.easeInOut(duration: 0.3)) {
                        proxy.scrollTo(target.id, anchor: .center)
                    }
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
