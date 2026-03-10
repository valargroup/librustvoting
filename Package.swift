// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "ZcashVotingFFI",
    platforms: [
        .iOS(.v16),
        .macOS(.v12),
    ],
    products: [
        .library(
            name: "ZcashVotingFFI",
            targets: ["ZcashVotingFFI"]
        )
    ],
    targets: [
        .binaryTarget(
            name: "zcash_voting_ffiFFI",
            url: "https://github.com/valargroup/librustvoting/releases/download/0.4.0/zcash_voting_ffiFFI.xcframework.zip",
            checksum: "d35e162aca18d13e4ffd81369155f5fe34c7138c72fd1d83060003f24cc35dc6"
        ),
        .target(
            name: "ZcashVotingFFI",
            dependencies: ["zcash_voting_ffiFFI"],
            path: "Sources/ZcashVotingFFI"
        )
    ]
)
