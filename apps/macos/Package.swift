// swift-tools-version:5.9
//
// SDRMac — SwiftUI macOS app executable.
//
// This is a development-time SwiftPM package that wraps the app
// target + the SdrCoreKit local dep so we can `swift build` and
// iterate without an Xcode project during M5 scaffolding. The
// real `.app` bundle (Info.plist, entitlements, code signing,
// notarization) lives in `SDRMac.xcodeproj/` once M6 lands.
//
// SwiftPM cannot produce a signed/notarized `.app` bundle —
// `swift run` here launches the SwiftUI executable as a bare
// process, which is fine for engine wiring + UI iteration but
// not for shipping. Use the Xcode project for anything that
// needs a proper bundle.

import PackageDescription

let package = Package(
    // Filesystem / Swift-module name stays `SDRMac` to avoid
    // churn across test imports and SwiftPM plumbing. The
    // user-facing executable and bundle name below are
    // `sdr-rs`, matching the Linux binary and `com.sdr.rs.*`
    // desktop/bundle identifiers.
    name: "SDRMac",
    platforms: [
        .macOS(.v14),
    ],
    products: [
        // The product name becomes the Mach-O binary filename
        // inside the .app bundle. `sdr-rs` (with hyphen) is
        // valid for product names even though Swift target
        // names can't have hyphens.
        .executable(name: "sdr-rs", targets: ["SDRMac"]),
    ],
    dependencies: [
        .package(path: "Packages/SdrCoreKit"),
    ],
    targets: [
        .executableTarget(
            name: "SDRMac",
            dependencies: [
                .product(name: "SdrCoreKit", package: "SdrCoreKit"),
            ],
            path: "SDRMac",
            exclude: [
                "Resources",
                "Entitlements",
            ]
        ),
        .testTarget(
            name: "SDRMacTests",
            dependencies: ["SDRMac"],
            path: "SDRMacTests"
        ),
    ]
)
