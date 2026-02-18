import SwiftUI
import ComposableArchitecture
import Generated
import UIComponents
import VotingModels

struct ResultsView: View {
    @Environment(\.colorScheme) var colorScheme

    let store: StoreOf<Voting>

    var body: some View {
        WithPerceptionTracking {
            ScrollView {
                VStack(spacing: 16) {
                    // Round header card
                    roundHeaderCard()

                    // Section header
                    HStack {
                        Text("Results")
                            .zFont(.semiBold, size: 18, style: Design.Text.primary)
                        Spacer()
                    }

                    if store.isLoadingTallyResults {
                        HStack(spacing: 8) {
                            ProgressView()
                            Text("Loading results...")
                                .zFont(.regular, size: 14, style: Design.Text.secondary)
                        }
                        .padding(.top, 20)
                    } else {
                        // Per-proposal result cards
                        ForEach(Array(store.votingRound.proposals.enumerated()), id: \.element.id) { index, proposal in
                            proposalResultCard(proposal: proposal, index: index)
                        }
                    }
                }
                .padding(.horizontal, 24)
                .padding(.bottom, 24)
            }
            .navigationTitle("Governance")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .navigationBarLeading) {
                    Button {
                        store.send(.dismissFlow)
                    } label: {
                        Image(systemName: "xmark")
                    }
                }
            }
        }
    }

    // MARK: - Round Header

    @ViewBuilder
    private func roundHeaderCard() -> some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(store.votingRound.title)
                .zFont(.semiBold, size: 18, style: Design.Text.primary)

            // Status pill
            Text("Finalized")
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(Color.accentColor)
                .padding(.horizontal, 10)
                .padding(.vertical, 4)
                .background(Color.accentColor.opacity(0.12))
                .clipShape(Capsule())

            // Detail pills
            HStack(spacing: 0) {
                detailPill(
                    label: "Snapshot",
                    value: store.votingRound.snapshotDate.formatted(date: .abbreviated, time: .omitted)
                )
                Spacer()
                detailPill(
                    label: "Ended",
                    value: store.votingRound.votingEnd.formatted(date: .abbreviated, time: .omitted)
                )
                Spacer()
                detailPill(
                    label: "Proposals",
                    value: "\(store.votingRound.proposals.count)"
                )
            }
        }
        .padding(16)
        .background(Design.Surfaces.bgPrimary.color(colorScheme))
        .clipShape(RoundedRectangle(cornerRadius: 14))
        .overlay(
            RoundedRectangle(cornerRadius: 14)
                .stroke(Design.Surfaces.strokeSecondary.color(colorScheme), lineWidth: 1)
        )
        .padding(.top, 8)
    }

    @ViewBuilder
    private func detailPill(label: String, value: String) -> some View {
        VStack(spacing: 2) {
            Text(label)
                .font(.system(size: 10, weight: .medium))
                .foregroundStyle(.secondary)
            Text(value)
                .font(.system(size: 12, weight: .semibold, design: .monospaced))
                .foregroundStyle(Design.Text.primary.color(colorScheme))
        }
    }

    // MARK: - Proposal Result Card

    @ViewBuilder
    private func proposalResultCard(proposal: Proposal, index: Int) -> some View {
        let tally = store.tallyResults[proposal.id]
        let entries = tally?.entries ?? []
        let totalAmount = entries.reduce(UInt64(0)) { $0 + $1.amount }

        VStack(alignment: .leading, spacing: 10) {
            // Header: number badge + title
            HStack(spacing: 8) {
                Text(String(format: "%02d", index + 1))
                    .zFont(.semiBold, size: 11, style: Design.Text.secondary)
                    .padding(.horizontal, 6)
                    .padding(.vertical, 2)
                    .background(Color.secondary.opacity(0.12))
                    .clipShape(RoundedRectangle(cornerRadius: 4))

                Text(proposal.title)
                    .zFont(.semiBold, size: 15, style: Design.Text.primary)
                    .lineLimit(2)
            }

            // Result bars — use option labels from proposal.options when available
            ForEach(entries.sorted(by: { $0.decision < $1.decision }), id: \.decision) { entry in
                let label = optionLabel(for: entry.decision, proposal: proposal)
                resultBar(
                    label: label,
                    amount: entry.amount,
                    total: totalAmount,
                    color: colorForDecision(entry.decision)
                )
            }

            if entries.isEmpty {
                Text("No votes recorded")
                    .zFont(.medium, size: 13, style: Design.Text.tertiary)
            }

            // Total
            if totalAmount > 0 {
                Text("Total: \(totalAmount) ballots")
                    .zFont(.medium, size: 12, style: Design.Text.tertiary)
            }
        }
        .padding(16)
        .background(Design.Surfaces.bgPrimary.color(colorScheme))
        .clipShape(RoundedRectangle(cornerRadius: 14))
        .overlay(
            RoundedRectangle(cornerRadius: 14)
                .stroke(Design.Surfaces.strokeSecondary.color(colorScheme), lineWidth: 1)
        )
    }

    // MARK: - Result Bar

    @ViewBuilder
    private func resultBar(label: String, amount: UInt64, total: UInt64, color: Color) -> some View {
        let ratio = total > 0 ? Double(amount) / Double(total) : 0

        HStack(spacing: 8) {
            Text(label)
                .zFont(.medium, size: 13, style: Design.Text.secondary)
                .frame(width: 60, alignment: .leading)

            GeometryReader { geo in
                RoundedRectangle(cornerRadius: 3)
                    .fill(color.opacity(0.7))
                    .frame(width: geo.size.width * ratio)
            }
            .frame(height: 8)

            Text("\(amount)")
                .zFont(.medium, size: 13, style: Design.Text.primary)
                .frame(width: 50, alignment: .trailing)
        }
    }

    // MARK: - Helpers

    private func optionLabel(for decision: UInt32, proposal: Proposal) -> String {
        if let option = proposal.options.first(where: { $0.index == decision }) {
            return option.label
        }
        // Fallback for proposals without explicit options
        switch decision {
        case 0: return "Support"
        case 1: return "Oppose"
        default: return "Option \(decision)"
        }
    }

    private func colorForDecision(_ decision: UInt32) -> Color {
        switch decision {
        case 0: return .green
        case 1: return .red
        case 2: return .blue
        case 3: return .purple
        default: return .orange
        }
    }
}
