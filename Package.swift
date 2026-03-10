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
            url: "https://github.com/z-cale/librustvoting/releases/download/0.2.0/zcash_voting_ffiFFI.xcframework.zip",
            checksum: "929163da0f87a97d6eb40a55bd12ab0dddba310c881b24f527e6d3bc918f7e37"
        ),
        .target(
            name: "ZcashVotingFFI",
            dependencies: ["zcash_voting_ffiFFI"],
            path: "Sources/ZcashVotingFFI"
        )
    ]
)
