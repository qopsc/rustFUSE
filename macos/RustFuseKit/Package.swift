// swift-tools-version:5.9
import PackageDescription

// What the rustFUSE app and its File Provider extension share: the wire
// protocol to `rustfuse sync --provider-socket`, the socket client and the
// app group layout.
let package = Package(
    name: "RustFuseKit",
    platforms: [.macOS(.v13)],
    products: [.library(name: "RustFuseKit", targets: ["RustFuseKit"])],
    targets: [
        .target(name: "RustFuseKit"),
        .testTarget(name: "RustFuseKitTests", dependencies: ["RustFuseKit"], exclude: ["wire.json"]),
    ]
)
