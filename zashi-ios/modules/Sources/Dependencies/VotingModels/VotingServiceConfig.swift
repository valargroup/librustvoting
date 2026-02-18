import Foundation

/// CDN-hosted config listing vote servers and nullifier providers.
/// Fetched at startup from `VotingServiceConfig.cdnURL`.
/// A local override file (`voting-config-local.json` in the app bundle) takes priority
/// to simplify testing against a local chain.
public struct VotingServiceConfig: Codable, Equatable, Sendable {
    public let version: Int
    public let voteServers: [ServiceEndpoint]
    public let nullifierProviders: [ServiceEndpoint]

    public struct ServiceEndpoint: Codable, Equatable, Sendable {
        public let url: String
        public let label: String

        public init(url: String, label: String) {
            self.url = url
            self.label = label
        }
    }

    public init(version: Int, voteServers: [ServiceEndpoint], nullifierProviders: [ServiceEndpoint]) {
        self.version = version
        self.voteServers = voteServers
        self.nullifierProviders = nullifierProviders
    }

    enum CodingKeys: String, CodingKey {
        case version
        case voteServers = "vote_servers"
        case nullifierProviders = "nullifier_providers"
    }

    /// CDN URL for the production config (auto-deployed via Vercel).
    public static let cdnURL = URL(string: "https://zally-phi.vercel.app/voting-config.json")!

    /// Filename for a local override bundled in the app (takes priority over CDN).
    public static let localOverrideFilename = "voting-config-local.json"

    /// Default config used when both local override and CDN are unavailable.
    /// Points at the deployed dev server so TestFlight builds work without CDN.
    public static let fallback = VotingServiceConfig(
        version: 1,
        voteServers: [ServiceEndpoint(url: "http://46.101.255.48:1318", label: "Primary")],
        nullifierProviders: [ServiceEndpoint(url: "http://46.101.255.48:3000", label: "Primary")]
    )
}
