// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "bite-helper",
    platforms: [.macOS(.v13)],
    targets: [
        .executableTarget(
            name: "bite-helper",
            path: "Sources/BiteBridge"
        ),
        .testTarget(
            name: "BiteBridgeTests",
            dependencies: ["bite-helper"],
            path: "Tests/BiteBridgeTests"
        )
    ],
    swiftLanguageVersions: [.v5]
)
