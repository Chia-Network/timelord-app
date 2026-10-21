// swift-tools-version: 5.10
import PackageDescription

let package = Package(
    name: "TimelordMac",
    platforms: [.macOS(.v14)],
    products: [.executable(name: "TimelordMac", targets: ["TimelordMac"])],
    targets: [
        .target(name: "TimelordPresentation"),
        .executableTarget(name: "TimelordMac", dependencies: ["TimelordPresentation"]),
        .testTarget(name: "TimelordPresentationTests", dependencies: ["TimelordPresentation"]),
        .testTarget(name: "TimelordMacTests", dependencies: ["TimelordMac", "TimelordPresentation"])
    ]
)
