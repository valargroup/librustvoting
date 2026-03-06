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
            url: "https://github.com/z-cale/librustvoting/releases/download/0.1.0/zcash_voting_ffiFFI.xcframework.zip",
            checksum: "38dd3602f6d3766e1a568035a2ec3635efe56888aaf96678b77164dbec51af9a"
        ),
        .target(
            name: "ZcashVotingFFI",
            dependencies: ["zcash_voting_ffiFFI"],
            path: "Sources/ZcashVotingFFI"
        )
    ]
)
