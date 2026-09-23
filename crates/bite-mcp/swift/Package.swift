// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "bite",
    platforms: [.macOS(.v13)],
    targets: [
        // Apple-side acquisition + mutations shared by the helper and the crawler
        .target(name: "BiteCrawlCore", path: "Sources/BiteCrawlCore"),
        // MCP helper (dispatcher + all bridges)
        .executableTarget(
            name: "bite-helper",
            dependencies: ["BiteCrawlCore"],
            path: "Sources/BiteHelper"
        ),
        // standalone detached crawler process
        .executableTarget(
            name: "bite-crawl",
            dependencies: ["BiteCrawlCore"],
            path: "Sources/BiteCrawlMain"
        ),
        .testTarget(
            name: "BiteBridgeTests",
            dependencies: ["bite-helper"],
            path: "Tests/BiteBridgeTests"
        ),
    ],
    swiftLanguageVersions: [.v5]
)
