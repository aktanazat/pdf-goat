// swift-tools-version: 6.2

import PackageDescription

let package = Package(
    name: "PDFGoat",
    platforms: [
        .macOS(.v15),
    ],
    products: [
        .executable(name: "PDFGoat", targets: ["PDFGoat"]),
    ],
    targets: [
        .executableTarget(
            name: "PDFGoat",
            exclude: ["Resources"]
        ),
        .testTarget(
            name: "PDFGoatTests",
            dependencies: ["PDFGoat"]
        ),
    ],
    swiftLanguageModes: [.v6]
)
