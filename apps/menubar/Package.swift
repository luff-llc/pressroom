// swift-tools-version: 6.0
// The menu bar item for Pressroom. A client of the local daemon (contracts/local-v1.md);
// it never does the daemon's job. No dependencies.
import PackageDescription

let package = Package(
    name: "PressroomMenuBar",
    platforms: [.macOS(.v15)],
    products: [
        .library(name: "PressroomKit", targets: ["PressroomKit"]),
        .executable(name: "PressroomMenuBar", targets: ["PressroomMenuBar"]),
    ],
    targets: [
        .target(name: "PressroomKit"),
        .executableTarget(name: "PressroomMenuBar", dependencies: ["PressroomKit"]),
        .testTarget(name: "PressroomKitTests", dependencies: ["PressroomKit"]),
    ],
    swiftLanguageModes: [.v6]
)
