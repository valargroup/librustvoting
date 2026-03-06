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
            checksum: "68f317ca62efc5fab5ab8d6880ab14ed059d567e708f7b372e96980c56cada85"
        ),
        .target(
            name: "ZcashVotingFFI",
            dependencies: ["zcash_voting_ffiFFI"],
            path: "Sources/ZcashVotingFFI"
        )
    ]
)
