import SwiftUI
import ComposableArchitecture
import Generated
import UIComponents

struct VotingErrorView: View {
    @Environment(\.colorScheme) var colorScheme

    let store: StoreOf<Voting>
    let errorMessage: String

    var body: some View {
        WithPerceptionTracking {
            VStack(spacing: 0) {
                Spacer()

                VStack(spacing: 24) {
                    // Icon
                    ZStack {
                        Circle()
                            .fill(Color.red.opacity(0.12))
                            .frame(width: 72, height: 72)
                        Image(systemName: "exclamationmark.circle")
                            .font(.system(size: 32))
                            .foregroundStyle(.red)
                    }

                    // Title
                    Text("Something Went Wrong")
                        .zFont(.semiBold, size: 22, style: Design.Text.primary)

                    // Description
                    Text("Unable to load governance proposals. Please try again.")
                        .zFont(.regular, size: 15, style: Design.Text.secondary)
                        .multilineTextAlignment(.center)
                        .padding(.horizontal, 32)

                    // Error detail card
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Error details")
                            .zFont(.medium, size: 13, style: Design.Text.tertiary)
                        Text(errorMessage)
                            .zFont(.regular, size: 14, style: Design.Text.secondary)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(14)
                    .background(Design.Text.secondary.color(colorScheme).opacity(0.06))
                    .clipShape(RoundedRectangle(cornerRadius: 12))
                    .padding(.horizontal, 24)
                }

                Spacer()

                // Retry + Dismiss buttons
                VStack(spacing: 12) {
                    ZashiButton("Retry") {
                        store.send(.initialize)
                    }

                    ZashiButton("Dismiss", type: .ghost) {
                        store.send(.dismissFlow)
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
}
