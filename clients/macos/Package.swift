// swift-tools-version: 6.0
import PackageDescription

let package = Package(
    name: "WatchcatClient",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "Watchcat", targets: ["WatchcatApp"]),
    ],
    targets: [
        .executableTarget(
            name: "WatchcatApp",
            path: "Sources/WatchcatApp",
            exclude: ["Resources"]
        ),
        .testTarget(
            name: "WatchcatAppTests",
            dependencies: ["WatchcatApp"],
            path: "Tests/WatchcatAppTests"
        ),
    ]
)
