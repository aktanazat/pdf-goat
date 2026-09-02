#!/usr/bin/env swift

import AppKit
import ApplicationServices
import CoreGraphics
import CryptoKit
import Darwin
import Foundation
import ScreenCaptureKit

let pollNS: UInt64 = 20_000_000
let footprintDelayNS: UInt64 = 750_000_000
let footprintToleranceNS: UInt64 = 25_000_000
let windowSize = CGSize(width: 1_200, height: 800)
let timeoutNS: UInt64 = 15_000_000_000
let schemaVersion = 3

struct BenchError: Error, CustomStringConvertible {
    let code: String
    let detail: String
    var description: String { "\(code): \(detail)" }
}

struct UsageError: Error, CustomStringConvertible {
    let description: String
}
nonisolated(unsafe) var caughtSignal: Int32 = 0

func benchmarkSignalHandler(_ signal: Int32) {
    caughtSignal = signal
}

func installSignalHandlers() {
    caughtSignal = 0
    Darwin.signal(SIGINT, benchmarkSignalHandler)
    Darwin.signal(SIGTERM, benchmarkSignalHandler)
}

func checkInterruption() throws {
    if caughtSignal != 0 {
        throw BenchError(code: "operator_interference", detail: "received signal \(caughtSignal)")
    }
}


let timebase: mach_timebase_info_data_t = {
    var value = mach_timebase_info_data_t()
    mach_timebase_info(&value)
    return value
}()

func nowNS() -> UInt64 {
    mach_continuous_time() * UInt64(timebase.numer) / UInt64(timebase.denom)
}

func sleep(until deadline: UInt64) {
    while nowNS() < deadline {
        Thread.sleep(forTimeInterval: min(Double(deadline - nowNS()) / 1_000_000_000, 0.02))
    }
}

func canonical(_ path: String) -> URL {
    URL(fileURLWithPath: path).standardizedFileURL.resolvingSymlinksInPath()
}

func digest(_ data: Data) -> String {
    SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
}

func digestFile(_ path: String) throws -> String {
    let handle = try FileHandle(forReadingFrom: URL(fileURLWithPath: path))
    defer { try? handle.close() }
    var hasher = SHA256()
    while let data = try handle.read(upToCount: 1_048_576), !data.isEmpty { hasher.update(data: data) }
    return hasher.finalize().map { String(format: "%02x", $0) }.joined()
}

func jsonData<Value: Encodable>(_ value: Value, pretty: Bool = false) throws -> Data {
    let encoder = JSONEncoder()
    var formatting: JSONEncoder.OutputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    if pretty { formatting.insert(.prettyPrinted) }
    encoder.outputFormatting = formatting
    encoder.keyEncodingStrategy = .convertToSnakeCase
    return try encoder.encode(value)
}

func jsonLine<Value: Encodable>(_ value: Value) throws -> Data {
    var data = try jsonData(value)
    data.append(0x0a)
    return data
}

struct CommandResult {
    let status: Int32
    let stdout: String
    let stderr: String
}

func command(_ executable: String, _ arguments: [String]) throws -> CommandResult {
    let process = Process()
    let stdout = Pipe()
    let stderr = Pipe()
    process.executableURL = URL(fileURLWithPath: executable)
    process.arguments = arguments
    process.standardOutput = stdout
    process.standardError = stderr
    try process.run()
    process.waitUntilExit()
    return CommandResult(
        status: process.terminationStatus,
        stdout: String(decoding: stdout.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self),
        stderr: String(decoding: stderr.fileHandleForReading.readDataToEndOfFile(), as: UTF8.self)
    )
}

func sysctlString(_ name: String) -> String? {
    var size = 0
    guard sysctlbyname(name, nil, &size, nil, 0) == 0, size > 1 else { return nil }
    var bytes = [CChar](repeating: 0, count: size)
    guard sysctlbyname(name, &bytes, &size, nil, 0) == 0 else { return nil }
    return String(decoding: bytes.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
}

func sysctlUInt64(_ name: String) -> UInt64? {
    var value: UInt64 = 0
    var size = MemoryLayout<UInt64>.size
    return sysctlbyname(name, &value, &size, nil, 0) == 0 ? value : nil
}

// MARK: Deterministic fixtures

struct Marker: Codable {
    let schemaVersion: Int
    let arrangement: String
    let red: [Int]
    let green: [Int]
    let blue: [Int]
    let yellow: [Int]
    let channelTolerance: Int
    let minimumCellPixels: Int
}

// How a trial decides the document is on screen. `marker` looks for the generated four-colour
// marker, `content` for white paper carrying dark ink. Absent in a manifest means `marker`.
enum Readiness: String, Codable {
    case marker
    case content

    var metric: String { "request_to_confirmed_visible_\(rawValue)" }
}

// A document entry either names a generated fixture (`path` nil, bytes rebuilt from `generated`)
// or points at an external file through `path`, absolute or relative to the manifest directory.
// Both carry sha256 and byte_count, so the corpus stays content addressed either way.
struct Fixture: Codable {
    let documentId: String
    let fileName: String
    let path: String?
    let sha256: String?
    let byteCount: Int?
    let pageCount: Int
    let readiness: Readiness?

    var isGenerated: Bool { path == nil }
}

struct Manifest: Codable {
    let generatorSchemaVersion: Int
    let marker: Marker
    let documents: [Fixture]
}

let decoder: JSONDecoder = {
    let value = JSONDecoder()
    value.keyDecodingStrategy = .convertFromSnakeCase
    return value
}()

func manifestPath() -> String {
    URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("corpus.json").path
}

func loadManifest(_ path: String = manifestPath()) throws -> Manifest {
    try decoder.decode(Manifest.self, from: Data(contentsOf: URL(fileURLWithPath: path)))
}

func pdfColor(_ rgb: [Int]) -> String {
    rgb.map { String(format: "%.6f", locale: Locale(identifier: "en_US_POSIX"), Double($0) / 255) }.joined(separator: " ")
}

func imageBytes(page: Int) -> Data {
    var state = UInt32(0x9e37_79b9) ^ UInt32(page &* 0x45d9f3b)
    var data = Data(capacity: 256 * 144 * 3)
    for _ in 0..<(256 * 144) {
        state ^= state << 13; state ^= state >> 17; state ^= state << 5
        let base = UInt8(72 + state % 132)
        data.append(base)
        data.append(UInt8(min(220, Int(base) + Int((state >> 8) % 13))))
        data.append(UInt8(max(48, Int(base) - Int((state >> 16) % 11))))
    }
    return data
}

func stream(_ bytes: Data, dictionary: String = "") -> Data {
    var data = Data("<< /Length \(bytes.count)\(dictionary) >>\nstream\n".utf8)
    data.append(bytes)
    data.append(Data("\nendstream".utf8))
    return data
}

func content(page: Int, raster: Bool, marker: Marker) -> Data {
    var text = "q\n0.975 g 0 0 612 792 re f\n0.82 G 1 w 30 30 552 732 re S\n"
    if page == 1 {
        let x = 54, y = 700, s = 32
        text += "\(pdfColor(marker.red)) rg \(x) \(y + s) \(s) \(s) re f\n"
        text += "\(pdfColor(marker.green)) rg \(x + s) \(y + s) \(s) \(s) re f\n"
        text += "\(pdfColor(marker.blue)) rg \(x) \(y) \(s) \(s) re f\n"
        text += "\(pdfColor(marker.yellow)) rg \(x + s) \(y) \(s) \(s) re f\n"
    }
    if raster {
        text += "q 500 0 0 281 56 235 cm /Im1 Do Q\n"
    } else {
        for row in 0..<26 {
            text += String(format: "%.3f G %.1f w 58 %d m %d %d l S\n", locale: Locale(identifier: "en_US_POSIX"), 0.18 + Double(row % 7) * 0.055, 0.5 + Double(row % 3) * 0.25, 120 + row * 20, 540 - (row % 5) * 24, 120 + row * 20)
        }
    }
    return Data((text + "Q\n").utf8)
}

func makePDF(pages: Int, mixed: Bool, marker: Marker) -> Data {
    var objects: [Int: Data] = [
        1: Data("<< /Type /Catalog /Pages 2 0 R >>".utf8),
        3: Data("<< /Title (PDF Goat deterministic benchmark fixture) /Producer (PDF Goat benchmark schema 1) /CreationDate (D:20260901000000Z) /ModDate (D:20260901000000Z) >>".utf8),
    ]
    var pageNumbers: [Int] = []
    var next = 4
    for page in 1...pages {
        let pageObject = next
        let contentObject = next + 1
        let raster = mixed && page.isMultiple(of: 2)
        let imageObject = raster ? next + 2 : nil
        next += raster ? 3 : 2
        pageNumbers.append(pageObject)
        let resources = imageObject.map { "<< /XObject << /Im1 \($0) 0 R >> >>" } ?? "<< >>"
        objects[pageObject] = Data("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources \(resources) /Contents \(contentObject) 0 R >>".utf8)
        objects[contentObject] = stream(content(page: page, raster: raster, marker: marker))
        if let imageObject {
            objects[imageObject] = stream(imageBytes(page: page), dictionary: " /Type /XObject /Subtype /Image /Width 256 /Height 144 /ColorSpace /DeviceRGB /BitsPerComponent 8")
        }
    }
    objects[2] = Data("<< /Type /Pages /Count \(pages) /Kids [\(pageNumbers.map { "\($0) 0 R" }.joined(separator: " "))] >>".utf8)
    let maximum = next - 1
    var result = Data("%PDF-1.4\n%\u{00e2}\u{00e3}\u{00cf}\u{00d3}\n".utf8)
    var offsets = [Int](repeating: 0, count: maximum + 1)
    for number in 1...maximum {
        offsets[number] = result.count
        result.append(Data("\(number) 0 obj\n".utf8))
        result.append(objects[number]!)
        result.append(Data("\nendobj\n".utf8))
    }
    let xref = result.count
    result.append(Data("xref\n0 \(maximum + 1)\n0000000000 65535 f \n".utf8))
    for number in 1...maximum { result.append(Data(String(format: "%010d 00000 n \n", offsets[number]).utf8)) }
    let identifier = "7064662d676f61742d62656e63682d31"
    result.append(Data("trailer\n<< /Size \(maximum + 1) /Root 1 0 R /Info 3 0 R /ID [<\(identifier)><\(identifier)>] >>\nstartxref\n\(xref)\n%%EOF\n".utf8))
    return result
}

func generated(_ manifest: Manifest) -> [String: Data] {
    [
        "tiny-3-page.pdf": makePDF(pages: 3, mixed: false, marker: manifest.marker),
        "mixed-12-page.pdf": makePDF(pages: 12, mixed: true, marker: manifest.marker),
    ]
}

// Digest and byte count as the manifest declares them, for the message an operator pastes back.
func declared(_ fixture: Fixture) -> String {
    "\(fixture.sha256 ?? "no sha256")/\(fixture.byteCount.map(String.init) ?? "no byte_count")"
}

func checkedCorpus(_ manifest: Manifest) throws -> [String: Data] {
    let files = generated(manifest)
    for fixture in manifest.documents where fixture.isGenerated {
        guard let data = files[fixture.fileName], digest(data) == fixture.sha256, data.count == fixture.byteCount else {
            let actual = files[fixture.fileName].map { "\(digest($0))/\($0.count)" } ?? "missing"
            throw BenchError(code: "corpus_mismatch", detail: "\(fixture.fileName) expected \(declared(fixture)), got \(actual)")
        }
    }
    return files
}

func generateMode(output: String) throws {
    let manifest = try loadManifest()
    let files = try checkedCorpus(manifest)
    let directory = canonical(output)
    let fixtures = manifest.documents.filter(\.isGenerated)
    let outputs = fixtures.map {
        directory.appendingPathComponent($0.fileName)
    } + [directory.appendingPathComponent("corpus.json")]
    if let existing = outputs.first(where: { FileManager.default.fileExists(atPath: $0.path) }) {
        throw BenchError(code: "output_exists", detail: existing.path)
    }
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    for fixture in fixtures {
        try files[fixture.fileName]!.write(
            to: directory.appendingPathComponent(fixture.fileName), options: .atomic
        )
    }
    try Data(contentsOf: URL(fileURLWithPath: manifestPath())).write(
        to: directory.appendingPathComponent("corpus.json"), options: .atomic
    )
    print("generated \(fixtures.count) fixtures in \(directory.path)")
}

// MARK: Process, app, and host identity

struct BundleInfo: Decodable {
    let bundleId: String
    let shortVersion: String
    let bundleVersion: String
    let executableName: String

    enum CodingKeys: String, CodingKey {
        case bundleId = "CFBundleIdentifier"
        case shortVersion = "CFBundleShortVersionString"
        case bundleVersion = "CFBundleVersion"
        case executableName = "CFBundleExecutable"
    }
}

struct AppReceipt: Codable {
    let appId: String
    let role: String
    let bundlePath: String
    let bundleId: String
    let shortVersion: String
    let bundleVersion: String
    let infoPlistSha256: String
    let executablePath: String
    let executableSha256: String
    let architecture: String
    let signingId: String
    let teamId: String?
    let cdhash: String

    enum CodingKeys: String, CodingKey {
        case appId, role, bundlePath, bundleId, shortVersion, bundleVersion
        case infoPlistSha256, executablePath, executableSha256, architecture
        case signingId, teamId, cdhash
    }

    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        try values.encode(appId, forKey: .appId)
        try values.encode(role, forKey: .role)
        try values.encode(bundlePath, forKey: .bundlePath)
        try values.encode(bundleId, forKey: .bundleId)
        try values.encode(shortVersion, forKey: .shortVersion)
        try values.encode(bundleVersion, forKey: .bundleVersion)
        try values.encode(infoPlistSha256, forKey: .infoPlistSha256)
        try values.encode(executablePath, forKey: .executablePath)
        try values.encode(executableSha256, forKey: .executableSha256)
        try values.encode(architecture, forKey: .architecture)
        try values.encode(signingId, forKey: .signingId)
        if let teamId { try values.encode(teamId, forKey: .teamId) } else { try values.encodeNil(forKey: .teamId) }
        try values.encode(cdhash, forKey: .cdhash)
    }
}

struct App {
    let id: String
    let role: String
    let bundle: String
    let executable: String
    let executableHash: String
    let infoHash: String
    let receipt: AppReceipt
}

func field(_ name: String, in text: String) -> String? {
    text.split(separator: "\n").first { $0.hasPrefix("\(name)=") }.map { String($0.dropFirst(name.count + 1)) }
}

func identifyApp(id: String, role: String, path: String) throws -> App {
    let bundle = canonical(path)
    guard bundle.pathExtension == "app", !bundle.path.contains("/AppTranslocation/") else { throw BenchError(code: "identity_mismatch", detail: "invalid or translocated app path \(bundle.path)") }
    let infoPath = bundle.appendingPathComponent("Contents/Info.plist").path
    let infoData = try Data(contentsOf: URL(fileURLWithPath: infoPath))
    guard let info = try? PropertyListDecoder().decode(BundleInfo.self, from: infoData) else {
        throw BenchError(code: "identity_mismatch", detail: "incomplete Info.plist at \(infoPath)")
    }
    let executable = canonical(bundle.appendingPathComponent("Contents/MacOS/\(info.executableName)").path).path
    let codesign = try command("/usr/bin/codesign", ["-d", "--verbose=4", executable])
    let signingText = codesign.stdout + codesign.stderr
    guard codesign.status == 0, let signingID = field("Identifier", in: signingText), let cdhash = field("CDHash", in: signingText) else { throw BenchError(code: "identity_mismatch", detail: "missing code-sign identity for \(executable)") }
    let lipo = try command("/usr/bin/lipo", ["-archs", executable])
    guard lipo.status == 0, let architecture = lipo.stdout.split(whereSeparator: { $0.isWhitespace }).first else { throw BenchError(code: "identity_mismatch", detail: "missing architecture for \(executable)") }
    let executableHash = try digestFile(executable)
    let infoHash = digest(infoData)
    let team = field("TeamIdentifier", in: signingText).flatMap { $0 == "not set" ? nil : $0 }
    let receipt = AppReceipt(
        appId: id,
        role: role,
        bundlePath: bundle.path,
        bundleId: info.bundleId,
        shortVersion: info.shortVersion,
        bundleVersion: info.bundleVersion,
        infoPlistSha256: infoHash,
        executablePath: executable,
        executableSha256: executableHash,
        architecture: String(architecture),
        signingId: signingID,
        teamId: team,
        cdhash: cdhash
    )
    return App(id: id, role: role, bundle: bundle.path, executable: executable, executableHash: executableHash, infoHash: infoHash, receipt: receipt)
}

struct ProcessReceipt: Codable {
    let pid: pid_t
    let startTimeNs: UInt64
    let executablePath: String
}

struct ProcessID: Equatable {
    let pid: pid_t
    let startNS: UInt64
    let executable: String
    var receipt: ProcessReceipt { ProcessReceipt(pid: pid, startTimeNs: startNS, executablePath: executable) }
}

func processID(_ pid: pid_t) -> ProcessID? {
    var path = [CChar](repeating: 0, count: 4_096)
    guard proc_pidpath(pid, &path, UInt32(path.count)) > 0 else { return nil }
    var info = proc_bsdinfo()
    let size = Int32(MemoryLayout<proc_bsdinfo>.size)
    guard withUnsafeMutablePointer(to: &info, { proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, $0, size) }) == size else { return nil }
    let executable = String(decoding: path.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
    return ProcessID(pid: pid, startNS: UInt64(info.pbi_start_tvsec) * 1_000_000_000 + UInt64(info.pbi_start_tvusec) * 1_000, executable: canonical(executable).path)
}

func exactProcesses(_ executable: String) -> [ProcessID] {
    let capacity = max(64, Int(proc_listallpids(nil, 0)))
    var pids = [pid_t](repeating: 0, count: capacity)
    let bytes = pids.withUnsafeMutableBytes { proc_listallpids($0.baseAddress, Int32($0.count)) }
    guard bytes > 0 else { return [] }
    return pids.prefix(Int(bytes) / MemoryLayout<pid_t>.size).compactMap(processID).filter { $0.executable == executable }
}

func requireProcess(_ expected: ProcessID) throws {
    try checkInterruption()
    guard let current = processID(expected.pid) else { throw BenchError(code: "process_exited", detail: "PID \(expected.pid) exited") }
    guard current == expected else { throw BenchError(code: "process_replaced", detail: "PID \(expected.pid) changed start identity or path") }
}

func footprint(_ pid: pid_t) -> UInt64? {
    var usage = rusage_info_v4()
    let status = withUnsafeMutablePointer(to: &usage) { pointer in
        pointer.withMemoryRebound(to: (rusage_info_t?).self, capacity: 1) { proc_pid_rusage(pid, RUSAGE_INFO_V4, $0) }
    }
    return status == 0 ? usage.ri_phys_footprint : nil
}

func thermalStateName() -> String {
    switch ProcessInfo.processInfo.thermalState {
    case .nominal: "nominal"
    case .fair: "fair"
    case .serious: "serious"
    case .critical: "critical"
    @unknown default: "unknown"
    }
}

struct HostReceipt: Codable {
    let hardwareModel: String
    let cpu: String
    let memoryBytes: UInt64
    let osVersion: String
    let osBuild: String
    let displayPoints: [Int]
    let displayPixels: [Int]
    let backingScale: Double
    let refreshHz: Double
    let colorSpace: String
    let powerSource: String
    let lowPowerMode: Bool
    let thermalState: String
    let diskCachePurged: Bool
}

func host() throws -> HostReceipt {
    let display = CGMainDisplayID()
    let bounds = CGDisplayBounds(display)
    // CGDisplayPixelsWide/High return points on a Retina display, so the display mode owns the
    // pixel dimensions and the backing scale is derived from them.
    guard let mode = CGDisplayCopyDisplayMode(display) else {
        throw BenchError(code: "geometry_mismatch", detail: "no display mode for the main display")
    }
    let pixelWidth = mode.pixelWidth
    let power = (try? command("/usr/bin/pmset", ["-g", "batt"]).stdout).map { $0.contains("AC Power") ? "ac" : ($0.contains("Battery Power") ? "battery" : "unknown") } ?? "unknown"
    let colorName = CGDisplayCopyColorSpace(display).name.map { $0 as String } ?? "unknown"
    return HostReceipt(
        hardwareModel: sysctlString("hw.model") ?? "unknown",
        cpu: sysctlString("machdep.cpu.brand_string") ?? sysctlString("hw.model") ?? "unknown",
        memoryBytes: sysctlUInt64("hw.memsize") ?? 0,
        osVersion: ProcessInfo.processInfo.operatingSystemVersionString,
        osBuild: sysctlString("kern.osversion") ?? "unknown",
        displayPoints: [Int(bounds.width), Int(bounds.height)],
        displayPixels: [pixelWidth, mode.pixelHeight],
        backingScale: bounds.width > 0 ? Double(pixelWidth) / Double(bounds.width) : 0,
        refreshHz: mode.refreshRate,
        colorSpace: colorName,
        powerSource: power,
        lowPowerMode: ProcessInfo.processInfo.isLowPowerModeEnabled,
        thermalState: thermalStateName(),
        diskCachePurged: false
    )
}

// MARK: Window geometry and marker detection

func axString(_ element: AXUIElement, _ attribute: CFString) -> String? {
    var raw: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, attribute, &raw) == .success else { return nil }
    if let value = raw as? String { return value }
    if let value = raw as? URL { return value.path }
    return nil
}

func axPoint(_ element: AXUIElement, _ attribute: CFString) -> CGPoint? {
    var raw: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, attribute, &raw) == .success,
          let raw,
          CFGetTypeID(raw) == AXValueGetTypeID() else { return nil }
    let value = unsafeBitCast(raw, to: AXValue.self)
    var point = CGPoint.zero
    return AXValueGetValue(value, .cgPoint, &point) ? point : nil
}

func axSize(_ element: AXUIElement, _ attribute: CFString) -> CGSize? {
    var raw: CFTypeRef?
    guard AXUIElementCopyAttributeValue(element, attribute, &raw) == .success,
          let raw,
          CFGetTypeID(raw) == AXValueGetTypeID() else { return nil }
    let value = unsafeBitCast(raw, to: AXValue.self)
    var size = CGSize.zero
    return AXValueGetValue(value, .cgSize, &size) ? size : nil
}

func standardWindows(_ pid: pid_t) -> [(AXUIElement, String?)] {
    var raw: CFTypeRef?
    guard AXUIElementCopyAttributeValue(AXUIElementCreateApplication(pid), kAXWindowsAttribute as CFString, &raw) == .success, let windows = raw as? [AXUIElement] else { return [] }
    return windows.compactMap { window in
        guard axString(window, kAXRoleAttribute as CFString) == (kAXWindowRole as String), axString(window, kAXSubroleAttribute as CFString) == (kAXStandardWindowSubrole as String) else { return nil }
        let document = axString(window, kAXDocumentAttribute as CFString).flatMap { value -> String? in
            if let url = URL(string: value), url.isFileURL { return canonical(url.path).path }
            return value.hasPrefix("/") ? canonical(value).path : nil
        }
        return (window, document)
    }
}

func setPosition(_ element: AXUIElement, _ point: CGPoint) throws {
    var point = point
    guard let wrapped = AXValueCreate(.cgPoint, &point),
          AXUIElementSetAttributeValue(element, kAXPositionAttribute as CFString, wrapped) == .success else {
        throw BenchError(code: "geometry_mismatch", detail: "app refused fixed window position")
    }
}

func setSize(_ element: AXUIElement, _ size: CGSize) throws {
    var size = size
    guard let wrapped = AXValueCreate(.cgSize, &size),
          AXUIElementSetAttributeValue(element, kAXSizeAttribute as CFString, wrapped) == .success else {
        throw BenchError(code: "geometry_mismatch", detail: "app refused 1200 by 800 window size")
    }
}

func rect(_ window: AXUIElement) -> CGRect? {
    guard let point = axPoint(window, kAXPositionAttribute as CFString), let size = axSize(window, kAXSizeAttribute as CFString) else { return nil }
    return CGRect(origin: point, size: size)
}

func sameRect(_ a: CGRect, _ b: CGRect) -> Bool {
    abs(a.minX - b.minX) <= 2 && abs(a.minY - b.minY) <= 2 && abs(a.width - b.width) <= 2 && abs(a.height - b.height) <= 2
}

func shareableWindow(_ pid: pid_t, near target: CGRect) throws -> SCWindow? {
    try shareableContent().windows.compactMap { window -> (SCWindow, CGFloat)? in
        guard window.owningApplication?.processID == pid, window.windowLayer == 0 else { return nil }
        let bounds = window.frame
        let distance = abs(bounds.minX - target.minX) + abs(bounds.minY - target.minY) + abs(bounds.width - target.width) + abs(bounds.height - target.height)
        return (window, distance)
    }.min { $0.1 < $1.1 }.flatMap { $0.1 <= 20 ? $0.0 : nil }
}

struct BoundWindow {
    let ax: AXUIElement
    let id: CGWindowID
    let scWindow: SCWindow
    let points: CGRect
    let pixels: [Int]
}

struct MemorySample: Codable {
    let tNs: UInt64
    let afterRequestNs: UInt64
    let bytes: UInt64
}

func settledWindow(process: ProcessID, pdf: String, target: CGRect, geometrySet: inout Bool) throws -> BoundWindow? {
    let windows = standardWindows(process.pid)
    if windows.count > 1 {
        throw BenchError(code: "restored_window", detail: "more than one standard document window appeared")
    }
    guard let (window, document) = windows.first else { return nil }
    if let document, canonical(document).path != canonical(pdf).path { return nil }
    if !geometrySet {
        try setPosition(window, target.origin)
        try setSize(window, target.size)
        geometrySet = true
    }
    guard let actual = rect(window),
          sameRect(actual, target),
          let scWindow = try shareableWindow(process.pid, near: actual) else { return nil }
    let scale = CGFloat(SCContentFilter(desktopIndependentWindow: scWindow).pointPixelScale)
    let bounds = scWindow.frame
    return BoundWindow(
        ax: window,
        id: scWindow.windowID,
        scWindow: scWindow,
        points: actual,
        pixels: [Int(bounds.minX * scale), Int(bounds.minY * scale), Int(bounds.width * scale), Int(bounds.height * scale)]
    )
}

func bindWindow(process: ProcessID, pdf: String, requestNS: UInt64, samples: inout [MemorySample]) throws -> BoundWindow {
    let display = CGDisplayBounds(CGMainDisplayID())
    guard display.width >= 1_280, display.height >= 880 else {
        throw BenchError(code: "geometry_mismatch", detail: "primary display is too small")
    }
    let target = CGRect(x: display.minX + 40, y: display.minY + 40, width: windowSize.width, height: windowSize.height)
    var geometrySet = false
    while nowNS() < requestNS + timeoutNS {
        try requireProcess(process)
        let t = nowNS()
        guard let bytes = footprint(process.pid) else {
            throw BenchError(code: "rusage_failed", detail: "memory sample failed before readiness")
        }
        samples.append(MemorySample(tNs: t, afterRequestNs: t - requestNS, bytes: bytes))
        if let window = try settledWindow(process: process, pdf: pdf, target: target, geometrySet: &geometrySet) {
            return window
        }
        sleep(until: t + pollNS)
    }
    if geometrySet {
        throw BenchError(code: "geometry_mismatch", detail: "window did not settle at fixed geometry")
    }
    throw BenchError(code: "window_timeout", detail: "document window did not appear")
}

struct Pixels {
    let width: Int
    let height: Int
    let bytes: [UInt8]
}

final class ShareableContentBox: @unchecked Sendable {
    var content: SCShareableContent?
    var error: Error?
}

func shareableContent() throws -> SCShareableContent {
    let box = ShareableContentBox()
    let semaphore = DispatchSemaphore(value: 0)
    SCShareableContent.getExcludingDesktopWindows(true, onScreenWindowsOnly: true) { content, error in
        box.content = content
        box.error = error
        semaphore.signal()
    }
    guard semaphore.wait(timeout: .now() + 2) == .success else {
        throw BenchError(code: "capture_denied", detail: "ScreenCaptureKit window query timed out")
    }
    if let error = box.error {
        throw BenchError(code: "capture_denied", detail: "ScreenCaptureKit window query failed: \(error.localizedDescription)")
    }
    guard let content = box.content else {
        throw BenchError(code: "capture_denied", detail: "ScreenCaptureKit window query returned no content")
    }
    return content
}

func shareableWindow(_ id: CGWindowID) throws -> SCWindow? {
    try shareableContent().windows.first { $0.windowID == id }
}

final class ScreenshotBox: @unchecked Sendable {
    var image: CGImage?
}

func rgbaPixels(from image: CGImage) -> Pixels? {
    var bytes = [UInt8](repeating: 0, count: image.width * image.height * 4)
    guard let context = CGContext(
        data: &bytes,
        width: image.width,
        height: image.height,
        bitsPerComponent: 8,
        bytesPerRow: image.width * 4,
        space: CGColorSpaceCreateDeviceRGB(),
        bitmapInfo: CGBitmapInfo.byteOrder32Big.rawValue | CGImageAlphaInfo.premultipliedLast.rawValue
    ) else { return nil }
    context.interpolationQuality = .none
    context.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
    return Pixels(width: image.width, height: image.height, bytes: bytes)
}

func capture(_ window: BoundWindow) -> Pixels? {
    let filter = SCContentFilter(desktopIndependentWindow: window.scWindow)
    let configuration = SCStreamConfiguration()
    configuration.width = Int(windowSize.width)
    configuration.height = Int(windowSize.height)
    configuration.showsCursor = false
    configuration.ignoreShadowsSingleWindow = true
    configuration.captureResolution = .nominal
    let box = ScreenshotBox()
    let semaphore = DispatchSemaphore(value: 0)
    SCScreenshotManager.captureImage(contentFilter: filter, configuration: configuration) { image, _ in
        box.image = image
        semaphore.signal()
    }
    guard semaphore.wait(timeout: .now() + 2) == .success, let image = box.image else { return nil }
    return rgbaPixels(from: image)
}

func matches(_ pixels: Pixels, x: Int, y: Int, color: [Int], tolerance: Int) -> Bool {
    guard x >= 0, y >= 0, x < pixels.width, y < pixels.height else { return false }
    let offset = (y * pixels.width + x) * 4
    return abs(Int(pixels.bytes[offset]) - color[0]) <= tolerance && abs(Int(pixels.bytes[offset + 1]) - color[1]) <= tolerance && abs(Int(pixels.bytes[offset + 2]) - color[2]) <= tolerance && pixels.bytes[offset + 3] >= 220
}

func patch(_ pixels: Pixels, x: Int, y: Int, color: [Int], marker: Marker) -> Double {
    let radius = marker.minimumCellPixels / 2
    var passing = 0, total = 0
    for row in (y - radius)..<(y + radius) { for column in (x - radius)..<(x + radius) { total += 1; if matches(pixels, x: column, y: row, color: color, tolerance: marker.channelTolerance) { passing += 1 } } }
    return Double(passing) / Double(total)
}

func markerScore(_ pixels: Pixels, marker: Marker) -> Double? {
    let minimum = marker.minimumCellPixels
    guard pixels.width > minimum * 4, pixels.height > minimum * 4 else { return nil }
    for y in stride(from: minimum, to: pixels.height - minimum * 2, by: 2) {
        for x in stride(from: minimum, to: pixels.width - minimum * 2, by: 2) where matches(pixels, x: x, y: y, color: marker.red, tolerance: marker.channelTolerance) {
            for separation in stride(from: minimum, through: min(96, pixels.width - x - minimum), by: 2) {
                guard y + separation + minimum < pixels.height,
                      matches(pixels, x: x + separation, y: y, color: marker.green, tolerance: marker.channelTolerance),
                      matches(pixels, x: x, y: y + separation, color: marker.blue, tolerance: marker.channelTolerance),
                      matches(pixels, x: x + separation, y: y + separation, color: marker.yellow, tolerance: marker.channelTolerance) else { continue }
                let scores = [patch(pixels, x: x, y: y, color: marker.red, marker: marker), patch(pixels, x: x + separation, y: y, color: marker.green, marker: marker), patch(pixels, x: x, y: y + separation, color: marker.blue, marker: marker), patch(pixels, x: x + separation, y: y + separation, color: marker.yellow, marker: marker)]
                if let score = scores.min(), score >= 0.75 { return score }
            }
        }
    }
    return nil
}

// The content detector, for real documents that carry no generated marker: white paper with dark
// ink on it. Thresholds, region of interest, and stage names come from the wave-2 open-time probe
// they were calibrated on. The ink gate is a count, not a fraction, because pst-geo page 1 is
// sparse vector art with 434 ink samples at 0.41 percent ink, which a 0.5 percent gate rejects on
// a drawn page.
let contentStride = 2
let contentRegion = (x: 0.28...0.92, y: 0.15...0.90)
let paperChannelMin = 235
let inkLumaMax = 105
let contentAlphaMin: UInt8 = 240
let minimumInkPixels = 24
let minimumPaperFraction = 0.5
let paperInset = 4

struct InkScore {
    var stage: String
    var paperWidth = 0
    var paperHeight = 0
    var inkPixels = 0
    var inkFraction = 0.0
    var paperFraction = 0.0
    var qualifyingRows = 0
    var qualifyingColumns = 0
    var alphaMax: UInt8 = 0
    var lumaHistogram: [Int] = []

    var passes: Bool { stage == "measured" && inkPixels >= minimumInkPixels && paperFraction >= minimumPaperFraction }
    var line: String {
        "stage=\(stage) paper=\(paperWidth)x\(paperHeight) rows=\(qualifyingRows) cols=\(qualifyingColumns) ink=\(inkPixels) inkFrac=\(String(format: "%.4f", inkFraction)) paperFrac=\(String(format: "%.4f", paperFraction)) alphaMax=\(alphaMax) luma16=\(lumaHistogram)"
    }
}

struct Sample {
    let red: Int, green: Int, blue: Int, alpha: UInt8

    init(_ pixels: Pixels, x: Int, y: Int) {
        let offset = (y * pixels.width + x) * 4
        red = Int(pixels.bytes[offset]); green = Int(pixels.bytes[offset + 1]); blue = Int(pixels.bytes[offset + 2])
        alpha = pixels.bytes[offset + 3]
    }

    var luma: Int { (red * 299 + green * 587 + blue * 114) / 1_000 }
    var opaque: Bool { alpha >= contentAlphaMin }
    var isPaper: Bool { red >= paperChannelMin && green >= paperChannelMin && blue >= paperChannelMin }
    var isInk: Bool { luma <= inkLumaMax }
}

// At least a quarter of `span` samples.
func quarterSpan(_ count: Int, of span: Int) -> Bool { count * 4 >= span }

func inkScore(_ pixels: Pixels) -> InkScore {
    let x0 = Int(Double(pixels.width) * contentRegion.x.lowerBound), x1 = Int(Double(pixels.width) * contentRegion.x.upperBound)
    let y0 = Int(Double(pixels.height) * contentRegion.y.lowerBound), y1 = Int(Double(pixels.height) * contentRegion.y.upperBound)
    guard x1 - x0 > 40, y1 - y0 > 40 else { return InkScore(stage: "roi_too_small") }
    // Rows and columns carrying paper across at least a quarter of their span bound the page.
    var rowPaper = [Int](repeating: 0, count: y1 - y0)
    var columnPaper = [Int](repeating: 0, count: x1 - x0)
    var alphaMax: UInt8 = 0
    var luma16 = [Int](repeating: 0, count: 16)
    for y in stride(from: y0, to: y1, by: contentStride) {
        for x in stride(from: x0, to: x1, by: contentStride) {
            let sample = Sample(pixels, x: x, y: y)
            alphaMax = max(alphaMax, sample.alpha)
            luma16[min(15, sample.luma / 16)] += 1
            if sample.opaque, sample.isPaper {
                rowPaper[y - y0] += 1
                columnPaper[x - x0] += 1
            }
        }
    }
    let rows = (0..<(y1 - y0)).filter { quarterSpan(rowPaper[$0], of: (x1 - x0) / contentStride) }
    let columns = (0..<(x1 - x0)).filter { quarterSpan(columnPaper[$0], of: (y1 - y0) / contentStride) }
    var score = InkScore(stage: "no_paper_span", qualifyingRows: rows.count, qualifyingColumns: columns.count, alphaMax: alphaMax, lumaHistogram: luma16)
    guard let top = rows.first, let bottom = rows.last, let left = columns.first, let right = columns.last else { return score }
    score.paperWidth = right - left + 1
    score.paperHeight = bottom - top + 1
    let inkX0 = x0 + left + paperInset, inkX1 = x0 + right - paperInset
    let inkY0 = y0 + top + paperInset, inkY1 = y0 + bottom - paperInset
    guard quarterSpan(score.paperWidth, of: x1 - x0), quarterSpan(score.paperHeight, of: y1 - y0), inkX1 > inkX0, inkY1 > inkY0 else {
        score.stage = "paper_too_small"
        return score
    }
    var ink = 0, paper = 0, total = 0
    for y in stride(from: inkY0, through: inkY1, by: contentStride) {
        for x in stride(from: inkX0, through: inkX1, by: contentStride) {
            let sample = Sample(pixels, x: x, y: y)
            total += 1
            guard sample.opaque else { continue }
            if sample.isInk { ink += 1 }
            if sample.isPaper { paper += 1 }
        }
    }
    guard total > 0 else {
        score.stage = "empty_paper_box"
        return score
    }
    score.stage = "measured"
    score.inkPixels = ink
    score.inkFraction = Double(ink) / Double(total)
    score.paperFraction = Double(paper) / Double(total)
    return score
}

// One frame's verdict: a score when the document is on screen, and the line an operator reads
// out of a timeout to see how far the frame got.
func frameScore(_ pixels: Pixels, readiness: Readiness, marker: Marker) -> (score: Double?, line: String) {
    switch readiness {
    case .marker:
        let score = markerScore(pixels, marker: marker)
        return (score, "stage=marker score=\(score.map { String(format: "%.3f", $0) } ?? "none")")
    case .content:
        let ink = inkScore(pixels)
        return (ink.passes ? ink.inkFraction : nil, ink.line)
    }
}

// Two consecutive passing frames with a steady score confirm readiness. A page still painting
// grows its ink fraction frame to frame, so a score that moved by more than a tenth restarts the
// candidate instead of confirming it.
let confirmTolerance = 0.1

struct Tracker {
    var candidate: (UInt64, Double)?
    var frames = 0
    mutating func add(time: UInt64, score: Double?) -> (UInt64, UInt64, Double, Double)? {
        frames += 1
        guard let score else { candidate = nil; return nil }
        if let candidate, abs(score - candidate.1) <= confirmTolerance * max(score, candidate.1) {
            return (candidate.0, time, candidate.1, score)
        }
        candidate = (time, score)
        return nil
    }
}

struct ReadinessReceipt: Codable {
    let detector: Readiness
    let framesExamined: Int
    let tFirstPassNs: UInt64
    let tConfirmNs: UInt64
    let latencyNs: UInt64
    let firstMatchScore: Double
    let confirmMatchScore: Double
}

struct FootprintReceipt: Codable {
    let api: String
    let atReadinessBytes: UInt64
    let dueNs: UInt64
    let actualNs: UInt64
    let offsetFromReadyNs: UInt64
    let latenessNs: UInt64
    let bytes: UInt64
    let startupPeakBytes: UInt64
    let timeSeries: [MemorySample]
}

struct Observation {
    let window: BoundWindow
    let firstWindowNS: UInt64
    let readiness: ReadinessReceipt
    let memory: FootprintReceipt
}

struct ReadySample {
    let receipt: ReadinessReceipt
    let atReadinessBytes: UInt64
}

struct DelayedFootprintSample {
    let dueNS: UInt64
    let actualNS: UInt64
    let latenessNS: UInt64
    let bytes: UInt64
}

func waitForReady(
    window: BoundWindow,
    process: ProcessID,
    requestNS: UInt64,
    readiness: Readiness,
    marker: Marker,
    samples: inout [MemorySample]
) throws -> ReadySample {
    var tracker = Tracker()
    var lastFrame = "no frame captured"
    var next = nowNS()
    while nowNS() < requestNS + timeoutNS {
        sleep(until: next)
        try requireProcess(process)
        guard let pixels = capture(window) else {
            throw BenchError(code: "capture_denied", detail: "window capture failed")
        }
        let t = nowNS()
        let frame = frameScore(pixels, readiness: readiness, marker: marker)
        lastFrame = frame.line
        let ready = tracker.add(time: t, score: frame.score)
        guard let bytes = footprint(process.pid) else {
            throw BenchError(code: "rusage_failed", detail: "startup sample failed")
        }
        samples.append(MemorySample(tNs: t, afterRequestNs: t - requestNS, bytes: bytes))
        if let ready {
            let receipt = ReadinessReceipt(
                detector: readiness,
                framesExamined: tracker.frames,
                tFirstPassNs: ready.0,
                tConfirmNs: ready.1,
                latencyNs: ready.1 - requestNS,
                firstMatchScore: ready.2,
                confirmMatchScore: ready.3
            )
            return ReadySample(receipt: receipt, atReadinessBytes: bytes)
        }
        next = t + pollNS
    }
    let code = "\(readiness.rawValue)_\(tracker.candidate == nil ? "timeout" : "unstable")"
    throw BenchError(code: code, detail: "two consecutive passing \(readiness.rawValue) frames were not observed; last frame \(lastFrame)")
}

func sampleDelayedFootprint(
    process: ProcessID,
    requestNS: UInt64,
    readyNS: UInt64,
    samples: inout [MemorySample]
) throws -> DelayedFootprintSample {
    let due = readyNS + footprintDelayNS
    while nowNS() < due {
        sleep(until: min(due, nowNS() + pollNS))
        let t = nowNS()
        if t < due {
            try requireProcess(process)
            guard let bytes = footprint(process.pid) else {
                throw BenchError(code: "rusage_failed", detail: "startup time-series sample failed")
            }
            samples.append(MemorySample(tNs: t, afterRequestNs: t - requestNS, bytes: bytes))
        }
    }
    try requireProcess(process)
    let actual = nowNS()
    let late = actual > due ? actual - due : 0
    guard late <= footprintToleranceNS else {
        throw BenchError(code: "sample_late", detail: "750 ms sample was \(late) ns late")
    }
    guard let bytes = footprint(process.pid) else {
        throw BenchError(code: "rusage_failed", detail: "750 ms sample failed")
    }
    samples.append(MemorySample(tNs: actual, afterRequestNs: actual - requestNS, bytes: bytes))
    return DelayedFootprintSample(dueNS: due, actualNS: actual, latenessNS: late, bytes: bytes)
}

func observe(process: ProcessID, pdf: DocumentReceipt, requestNS: UInt64, marker: Marker) throws -> Observation {
    var samples: [MemorySample] = []
    let window = try bindWindow(process: process, pdf: pdf.path, requestNS: requestNS, samples: &samples)
    let firstWindow = nowNS()
    let ready = try waitForReady(window: window, process: process, requestNS: requestNS, readiness: pdf.readiness, marker: marker, samples: &samples)
    let delayed = try sampleDelayedFootprint(process: process, requestNS: requestNS, readyNS: ready.receipt.tConfirmNs, samples: &samples)
    let memory = FootprintReceipt(
        api: "proc_pid_rusage.RUSAGE_INFO_V4.ri_phys_footprint",
        atReadinessBytes: ready.atReadinessBytes,
        dueNs: delayed.dueNS,
        actualNs: delayed.actualNS,
        offsetFromReadyNs: delayed.actualNS - ready.receipt.tConfirmNs,
        latenessNs: delayed.latenessNS,
        bytes: delayed.bytes,
        startupPeakBytes: samples.map(\.bytes).max() ?? delayed.bytes,
        timeSeries: samples
    )
    return Observation(window: window, firstWindowNS: firstWindow, readiness: ready.receipt, memory: memory)
}

struct CloseReceipt: Codable {
    let requestCount: Int
    let tRequestNs: UInt64
    let tConfirmNs: UInt64
    let windowGone: Bool
}

func close(_ window: BoundWindow, process: ProcessID) throws -> CloseReceipt {
    let requested = nowNS()
    var closeButton: CFTypeRef?
    guard AXUIElementCopyAttributeValue(window.ax, kAXCloseButtonAttribute as CFString, &closeButton) == .success,
          let closeButton,
          CFGetTypeID(closeButton) == AXUIElementGetTypeID() else {
        throw BenchError(code: "close_timeout", detail: "AX close button is unavailable")
    }
    let button = unsafeBitCast(closeButton, to: AXUIElement.self)
    guard AXUIElementPerformAction(button, kAXPressAction as CFString) == .success else {
        throw BenchError(code: "close_timeout", detail: "AX close action failed")
    }
    while nowNS() < requested + 3_000_000_000 {
        try requireProcess(process)
        if try shareableWindow(window.id) == nil {
            return CloseReceipt(requestCount: 1, tRequestNs: requested, tConfirmNs: nowNS(), windowGone: true)
        }
        sleep(until: nowNS() + pollNS)
    }
    throw BenchError(code: "close_timeout", detail: "window did not close")
}

func warmIdle(_ process: ProcessID) throws {
    let start = nowNS()
    while nowNS() - start < 500_000_000 {
        try requireProcess(process)
        guard standardWindows(process.pid).isEmpty else { throw BenchError(code: "window_lingered", detail: "warm process was not window-free for 500 ms") }
        sleep(until: nowNS() + pollNS)
    }
}

struct CleanupReceipt: Codable {
    let harnessOwnedPid: Bool
    let terminateSent: Bool
    let killSent: Bool
    let pidExited: Bool
}

func terminate(_ process: ProcessID) -> CleanupReceipt {
    guard let current = processID(process.pid) else { return CleanupReceipt(harnessOwnedPid: true, terminateSent: false, killSent: false, pidExited: true) }
    guard current == process else { return CleanupReceipt(harnessOwnedPid: true, terminateSent: false, killSent: false, pidExited: false) }
    Darwin.kill(process.pid, SIGTERM)
    let deadline = nowNS() + 5_000_000_000
    while nowNS() < deadline && processID(process.pid) != nil { sleep(until: nowNS() + pollNS) }
    if processID(process.pid) == nil { return CleanupReceipt(harnessOwnedPid: true, terminateSent: true, killSent: false, pidExited: true) }
    guard processID(process.pid) == process else { return CleanupReceipt(harnessOwnedPid: true, terminateSent: true, killSent: false, pidExited: false) }
    Darwin.kill(process.pid, SIGKILL)
    sleep(until: nowNS() + 100_000_000)
    return CleanupReceipt(harnessOwnedPid: true, terminateSent: true, killSent: true, pidExited: processID(process.pid) == nil)
}

// MARK: Raw run receipts

enum Lane: String, Codable {
    case fresh
    case warmPrime = "warm_prime"
    case warm
}

struct ScheduleItem: Codable, Equatable {
    let sequence: Int
    let appId: String
    let documentId: String
    let lane: Lane
    let repetition: Int
}

struct DocumentReceipt: Codable {
    let documentId: String
    let path: String
    let sha256: String
    let bytes: Int
    let pages: Int
    let readiness: Readiness
    let generatorSchema: Int
    let markerSchema: Int
}

struct RequestReceipt: Codable {
    let kind: String
    var count: Int
    var argv: [String]
    var exitStatus: Int32?
    var tRequestNs: UInt64?

    enum CodingKeys: String, CodingKey {
        case kind, count, argv, exitStatus, tRequestNs
    }

    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        try values.encode(kind, forKey: .kind)
        try values.encode(count, forKey: .count)
        try values.encode(argv, forKey: .argv)
        if let exitStatus { try values.encode(exitStatus, forKey: .exitStatus) } else { try values.encodeNil(forKey: .exitStatus) }
        if let tRequestNs { try values.encode(tRequestNs, forKey: .tRequestNs) } else { try values.encodeNil(forKey: .tRequestNs) }
    }
}

struct WindowReceipt: Codable {
    let windowId: CGWindowID
    let documentUrl: String
    let boundsPoints: [Int]
    let boundsPixels: [Int]
    let tSeenNs: UInt64
}

struct TrialReceipt: Codable {
    let record: String
    let schemaVersion: Int
    let sessionId: String
    let sequence: Int
    let attemptId: String
    let appId: String
    let documentId: String
    let lane: Lane
    let repetition: Int
    var status: String
    var failureCode: String?
    var failureDetail: String?
    var request: RequestReceipt
    var process: ProcessReceipt?
    var processGenerationId: String?
    var window: WindowReceipt?
    var readiness: ReadinessReceipt?
    var footprint: FootprintReceipt?
    var close: CloseReceipt?
    var cleanup: CleanupReceipt?

    enum CodingKeys: String, CodingKey {
        case record, schemaVersion, sessionId, sequence, attemptId, appId, documentId
        case lane, repetition, status, failureCode, failureDetail, request, process
        case processGenerationId, window, readiness, footprint, close, cleanup
    }

    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        try values.encode(record, forKey: .record)
        try values.encode(schemaVersion, forKey: .schemaVersion)
        try values.encode(sessionId, forKey: .sessionId)
        try values.encode(sequence, forKey: .sequence)
        try values.encode(attemptId, forKey: .attemptId)
        try values.encode(appId, forKey: .appId)
        try values.encode(documentId, forKey: .documentId)
        try values.encode(lane, forKey: .lane)
        try values.encode(repetition, forKey: .repetition)
        try values.encode(status, forKey: .status)
        if let failureCode { try values.encode(failureCode, forKey: .failureCode) } else { try values.encodeNil(forKey: .failureCode) }
        if let failureDetail { try values.encode(failureDetail, forKey: .failureDetail) } else { try values.encodeNil(forKey: .failureDetail) }
        try values.encode(request, forKey: .request)
        if let process { try values.encode(process, forKey: .process) } else { try values.encodeNil(forKey: .process) }
        try values.encodeIfPresent(processGenerationId, forKey: .processGenerationId)
        if let window { try values.encode(window, forKey: .window) } else { try values.encodeNil(forKey: .window) }
        if let readiness { try values.encode(readiness, forKey: .readiness) } else { try values.encodeNil(forKey: .readiness) }
        if let footprint { try values.encode(footprint, forKey: .footprint) } else { try values.encodeNil(forKey: .footprint) }
        if let close { try values.encode(close, forKey: .close) } else { try values.encodeNil(forKey: .close) }
        if let cleanup { try values.encode(cleanup, forKey: .cleanup) } else { try values.encodeNil(forKey: .cleanup) }
    }
}

final class RawWriter {
    let handle: FileHandle
    init(_ path: String) throws {
        try FileManager.default.createDirectory(at: URL(fileURLWithPath: path).deletingLastPathComponent(), withIntermediateDirectories: true)
        let descriptor = Darwin.open(path, O_WRONLY | O_CREAT | O_EXCL, S_IRUSR | S_IWUSR)
        guard descriptor >= 0 else { throw UsageError(description: "raw output exists or cannot be created: \(path)") }
        handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
    }
    func append<Value: Encodable>(_ value: Value) throws {
        try handle.write(contentsOf: jsonLine(value))
        try handle.synchronize()
    }
    func close() throws { try handle.close() }
}

func baseReceipt(session: String, item: ScheduleItem, kind: String) -> TrialReceipt {
    TrialReceipt(
        record: "trial",
        schemaVersion: schemaVersion,
        sessionId: session,
        sequence: item.sequence,
        attemptId: UUID().uuidString,
        appId: item.appId,
        documentId: item.documentId,
        lane: item.lane,
        repetition: item.repetition,
        status: "failed",
        failureCode: nil,
        failureDetail: nil,
        request: RequestReceipt(kind: kind, count: 0, argv: [], exitStatus: nil, tRequestNs: nil),
        process: nil,
        processGenerationId: nil,
        window: nil,
        readiness: nil,
        footprint: nil,
        close: nil,
        cleanup: nil
    )
}

func valid(_ receipt: inout TrialReceipt) {
    receipt.status = "valid"
    receipt.failureCode = nil
    receipt.failureDetail = nil
}

func fail(_ receipt: inout TrialReceipt, _ error: Error) {
    let failure = error as? BenchError ?? BenchError(code: "request_failed", detail: String(describing: error))
    receipt.status = "failed"
    receipt.failureCode = failure.code
    receipt.failureDetail = failure.detail
}

func store(_ observation: Observation, pdf: String, receipt: inout TrialReceipt) {
    receipt.window = WindowReceipt(
        windowId: observation.window.id,
        documentUrl: pdf,
        boundsPoints: [Int(observation.window.points.minX), Int(observation.window.points.minY), Int(observation.window.points.width), Int(observation.window.points.height)],
        boundsPixels: observation.window.pixels,
        tSeenNs: observation.firstWindowNS
    )
    receipt.readiness = observation.readiness
    receipt.footprint = observation.memory
}

func verify(_ app: App, pdf: DocumentReceipt) throws {
    guard try digestFile(app.executable) == app.executableHash,
          try digestFile(URL(fileURLWithPath: app.bundle).appendingPathComponent("Contents/Info.plist").path) == app.infoHash else { throw BenchError(code: "identity_mismatch", detail: "app identity changed after scheduling") }
    guard try digestFile(pdf.path) == pdf.sha256 else { throw BenchError(code: "corpus_mismatch", detail: "session PDF changed") }
}

final class WorkspaceLaunch: @unchecked Sendable {
    let completed = DispatchSemaphore(value: 0)
    var application: NSRunningApplication?
    var error: Error?
}

func launchFresh(app: App, pdf: DocumentReceipt) throws -> ProcessID {
    let configuration = NSWorkspace.OpenConfiguration()
    configuration.activates = true
    configuration.addsToRecentItems = false
    configuration.createsNewApplicationInstance = true
    configuration.promptsUserIfNeeded = false
    let result = WorkspaceLaunch()
    NSWorkspace.shared.open(
        [URL(fileURLWithPath: pdf.path)],
        withApplicationAt: URL(fileURLWithPath: app.bundle),
        configuration: configuration
    ) { application, error in
        result.application = application
        result.error = error
        result.completed.signal()
    }
    guard result.completed.wait(timeout: .now() + 5) == .success else {
        throw BenchError(code: "pid_timeout", detail: "NSWorkspace did not return the launched process")
    }
    if let error = result.error {
        throw BenchError(code: "request_failed", detail: error.localizedDescription)
    }
    guard let application = result.application,
          let identity = processID(application.processIdentifier),
          identity.executable == app.executable else {
        throw BenchError(code: "identity_mismatch", detail: "NSWorkspace returned the wrong process")
    }
    return identity
}

func cleanupFailedFresh(bound: ProcessID?) -> CleanupReceipt? {
    bound.map(terminate)
}

func fresh(session: String, item: ScheduleItem, app: App, pdf: DocumentReceipt, marker: Marker, keep: Bool) -> (TrialReceipt, ProcessID?) {
    var receipt = baseReceipt(session: session, item: item, kind: "workspace_open_new_instance")
    var bound: ProcessID?
    do {
        try checkInterruption()
        try verify(app, pdf: pdf)
        guard exactProcesses(app.executable).isEmpty else { throw BenchError(code: "preexisting_process", detail: "exact executable is already running") }
        sleep(until: nowNS() + 2_000_000_000)
        try checkInterruption()
        guard exactProcesses(app.executable).isEmpty else { throw BenchError(code: "preexisting_process", detail: "exact executable appeared during inter-trial gap") }
        let requested = nowNS()
        receipt.request = RequestReceipt(
            kind: "workspace_open_new_instance",
            count: 1,
            argv: ["NSWorkspace.open", app.bundle, pdf.path],
            exitStatus: nil,
            tRequestNs: requested
        )
        let identity = try launchFresh(app: app, pdf: pdf)
        receipt.request.exitStatus = 0
        bound = identity
        try verify(app, pdf: pdf)
        receipt.process = identity.receipt
        receipt.processGenerationId = "\(identity.pid)-\(identity.startNS)"
        let result = try observe(process: identity, pdf: pdf, requestNS: requested, marker: marker)
        store(result, pdf: pdf.path, receipt: &receipt)
        receipt.close = try close(result.window, process: identity)
        if keep {
            try warmIdle(identity)
            valid(&receipt)
            return (receipt, identity)
        }
        let cleanup = terminate(identity)
        receipt.cleanup = cleanup
        if cleanup.killSent || !cleanup.pidExited { throw BenchError(code: "cleanup_failed", detail: "fresh process did not terminate normally") }
        valid(&receipt)
        return (receipt, nil)
    } catch {
        receipt.cleanup = cleanupFailedFresh(bound: bound)
        fail(&receipt, error)
        return (receipt, nil)
    }
}

func warm(session: String, item: ScheduleItem, app: App, pdf: DocumentReceipt, marker: Marker, process: ProcessID, last: Bool) -> TrialReceipt {
    var receipt = baseReceipt(session: session, item: item, kind: "open_existing")
    let identity = process
    receipt.process = identity.receipt
    receipt.processGenerationId = "\(identity.pid)-\(identity.startNS)"
    do {
        try checkInterruption()
        try verify(app, pdf: pdf)
        let arguments = ["-a", app.bundle, pdf.path]
        let requested = nowNS()
        receipt.request = RequestReceipt(kind: "open_existing", count: 1, argv: ["/usr/bin/open"] + arguments, exitStatus: nil, tRequestNs: requested)
        let opened = try command("/usr/bin/open", arguments)
        receipt.request.exitStatus = opened.status
        guard opened.status == 0 else { throw BenchError(code: "request_failed", detail: "open exited \(opened.status)") }
        try requireProcess(identity)
        guard exactProcesses(app.executable) == [identity] else { throw BenchError(code: "process_replaced", detail: "warm request changed process generation") }
        let result = try observe(process: identity, pdf: pdf, requestNS: requested, marker: marker)
        store(result, pdf: pdf.path, receipt: &receipt)
        receipt.close = try close(result.window, process: identity)
        if last {
            let cleanup = terminate(identity)
            receipt.cleanup = cleanup
            if cleanup.killSent || !cleanup.pidExited { throw BenchError(code: "cleanup_failed", detail: "warm process did not terminate normally") }
        } else {
            try warmIdle(identity)
        }
        valid(&receipt)
    } catch {
        receipt.cleanup = terminate(identity)
        fail(&receipt, error)
    }
    return receipt
}

func schedule(apps: [App], documents: [DocumentReceipt], freshRuns: Int, warmRuns: Int) -> [ScheduleItem] {
    var result: [ScheduleItem] = []
    var sequence = 0
    for (documentIndex, document) in documents.enumerated() {
        for round in 0..<freshRuns {
            let offset = (round + documentIndex * 2) % apps.count
            for index in 0..<apps.count {
                result.append(ScheduleItem(sequence: sequence, appId: apps[(index + offset) % apps.count].id, documentId: document.documentId, lane: .fresh, repetition: round + 1))
                sequence += 1
            }
        }
    }
    for (documentIndex, document) in documents.enumerated() {
        for app in (documentIndex.isMultiple(of: 2) ? apps : Array(apps.reversed())) {
            result.append(ScheduleItem(sequence: sequence, appId: app.id, documentId: document.documentId, lane: .warmPrime, repetition: 0))
            sequence += 1
            for repetition in 1...warmRuns {
                result.append(ScheduleItem(sequence: sequence, appId: app.id, documentId: document.documentId, lane: .warm, repetition: repetition))
                sequence += 1
            }
        }
    }
    return result
}

func configuredApps(_ arguments: [String]) throws -> [App] {
    var apps = [try identifyApp(id: "pdf-goat", role: "subject", path: required("--pdf-goat", arguments))]
    for (flag, id) in [("--preview", "preview"), ("--pdfgear", "pdfgear"), ("--skim", "skim")] {
        if let path = try option(flag, arguments) {
            apps.append(try identifyApp(id: id, role: "competitor", path: path))
        }
    }
    guard Set(apps.map(\.bundle)).count == apps.count, Set(apps.map(\.executable)).count == apps.count else {
        throw BenchError(code: "identity_mismatch", detail: "duplicate explicit app path")
    }
    return apps
}

func copyDocuments(manifest: Manifest, corpusPath: String, destination: URL) throws -> [DocumentReceipt] {
    let corpus = canonical(corpusPath)
    var documents: [DocumentReceipt] = []
    for fixture in manifest.documents {
        let source = canonical(fixture.path.map { $0.hasPrefix("/") ? $0 : corpus.appendingPathComponent($0).path }
            ?? corpus.appendingPathComponent(fixture.fileName).path)
        let bytes = try source.resourceValues(forKeys: [.fileSizeKey]).fileSize ?? -1
        let sha256 = try digestFile(source.path)
        guard bytes == fixture.byteCount, sha256 == fixture.sha256 else {
            throw BenchError(code: "corpus_mismatch", detail: "\(source.path) is \(sha256)/\(bytes), manifest declares \(declared(fixture))")
        }
        let output = destination.appendingPathComponent(fixture.fileName)
        try FileManager.default.copyItem(at: source, to: output)
        guard try digestFile(output.path) == sha256 else {
            throw BenchError(code: "corpus_mismatch", detail: "copy changed \(fixture.fileName)")
        }
        documents.append(DocumentReceipt(
            documentId: fixture.documentId,
            path: output.path,
            sha256: sha256,
            bytes: bytes,
            pages: fixture.pageCount,
            readiness: fixture.readiness ?? .marker,
            generatorSchema: manifest.generatorSchemaVersion,
            markerSchema: manifest.marker.schemaVersion
        ))
    }
    return documents
}

struct RunCounts {
    var terminal = 0
    var measured = 0
    var failures = 0
    var aborted = false
}

func executeItem(
    _ item: ScheduleItem,
    session: String,
    apps: [App],
    documents: [DocumentReceipt],
    marker: Marker,
    warmRuns: Int,
    active: inout ProcessID?
) throws -> TrialReceipt {
    guard let app = apps.first(where: { $0.id == item.appId }),
          let pdf = documents.first(where: { $0.documentId == item.documentId }) else {
        throw UsageError(description: "scheduled app or document is missing")
    }
    switch item.lane {
    case .fresh:
        return fresh(session: session, item: item, app: app, pdf: pdf, marker: marker, keep: false).0
    case .warmPrime:
        let result = fresh(session: session, item: item, app: app, pdf: pdf, marker: marker, keep: true)
        active = result.1
        return result.0
    case .warm:
        guard let process = active else {
            var failed = baseReceipt(session: session, item: item, kind: "open_existing")
            fail(&failed, BenchError(code: "process_exited", detail: "warm prime left no process"))
            return failed
        }
        let last = item.repetition == warmRuns
        let receipt = warm(session: session, item: item, app: app, pdf: pdf, marker: marker, process: process, last: last)
        if last { active = nil }
        return receipt
    }
}

func execute(_ plan: [ScheduleItem], session: String, apps: [App], documents: [DocumentReceipt], marker: Marker, warmRuns: Int, writer: RawWriter) throws -> RunCounts {
    var counts = RunCounts()
    var active: ProcessID?
    defer {
        if let active { _ = terminate(active) }
    }
    for item in plan {
        let receipt = try executeItem(item, session: session, apps: apps, documents: documents, marker: marker, warmRuns: warmRuns, active: &active)
        try writer.append(receipt)
        counts.terminal += 1
        if receipt.status == "failed" {
            counts.failures += 1
            counts.aborted = true
            if let active { _ = terminate(active) }
            active = nil
            break
        }
        if item.lane != .warmPrime {
            counts.measured += 1
        }
    }
    if let active {
        _ = terminate(active)
    }
    active = nil
    return counts
}

struct HostStability {
    let power: Bool
    let display: Bool
    let lowPowerMode: Bool
    let thermalState: Bool

    var stable: Bool { power && display && lowPowerMode && thermalState }
}

func hostStateMatches(_ start: HostReceipt, _ end: HostReceipt) -> HostStability {
    HostStability(
        power: start.powerSource == end.powerSource,
        display: start.displayPoints == end.displayPoints &&
            start.displayPixels == end.displayPixels &&
            start.backingScale == end.backingScale,
        lowPowerMode: start.lowPowerMode == end.lowPowerMode,
        thermalState: start.thermalState == end.thermalState
    )
}
func removeSessionDirectory(_ directory: URL) -> Bool {
    do {
        try FileManager.default.removeItem(at: directory)
        return true
    } catch {
        return false
    }
}

struct HarnessReceipt: Codable {
    let scriptSha256: String
    let argv: [String]
    let pollIntervalMs: Int
    let readyFramesRequired: Int
    let footprintDelayMs: Int
    let footprintToleranceMs: Int
    let windowSizePoints: [Int]
    let freshRuns: Int
    let warmRuns: Int
}

// The metric is no longer one string per session: it is one per document, named by that
// document's readiness detector, and repeated per trial in `readiness.detector`.
struct ScopeReceipt: Codable {
    let freshLane: String
    let footprintOwner: String
    let pdfGoatScope: String
}

struct SessionReceipt: Codable {
    let record: String
    let schemaVersion: Int
    let sessionId: String
    let startedAtUtc: String
    let harness: HarnessReceipt
    let host: HostReceipt
    let scope: ScopeReceipt
    let apps: [AppReceipt]
    let documents: [DocumentReceipt]
    let schedule: [ScheduleItem]
}

struct SessionEndReceipt: Codable {
    let record: String
    let schemaVersion: Int
    let sessionId: String
    let finishedAtUtc: String
    let outcome: String
    let scheduledTrials: Int
    let terminalReceipts: Int
    let validMeasuredTrials: Int
    let failedTrials: Int
    let thermalStateEnd: String
    let lowPowerModeEnd: Bool
    let powerSourceUnchanged: Bool
    let displayUnchanged: Bool
    let lowPowerModeUnchanged: Bool
    let thermalStateUnchanged: Bool
    let cleanupComplete: Bool
}

func runMode(arguments: [String]) throws {
    guard AXIsProcessTrusted() else { throw BenchError(code: "geometry_mismatch", detail: "Accessibility permission is required") }
    guard CGPreflightScreenCaptureAccess() else { throw BenchError(code: "capture_denied", detail: "Screen Recording permission is required") }
    installSignalHandlers()
    let corpusPath = try required("--corpus", arguments)
    let output = canonical(try required("--output", arguments)).path
    let freshRuns = try positive("--fresh-runs", arguments, default: 5)
    let warmRuns = try positive("--warm-runs", arguments, default: 3)
    let apps = try configuredApps(arguments)
    let manifest = try loadManifest(canonical(corpusPath).appendingPathComponent("corpus.json").path)
    _ = try checkedCorpus(manifest)
    let session = UUID().uuidString
    let temporary = URL(fileURLWithPath: output).deletingLastPathComponent().appendingPathComponent("session-\(session)")
    try FileManager.default.createDirectory(at: temporary, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: temporary) }
    let documents = try copyDocuments(manifest: manifest, corpusPath: corpusPath, destination: temporary)
    let plan = schedule(apps: apps, documents: documents, freshRuns: freshRuns, warmRuns: warmRuns)
    let startHost = try host()
    let writer = try RawWriter(output)
    let harness = HarnessReceipt(
        scriptSha256: try digestFile(#filePath),
        argv: CommandLine.arguments,
        pollIntervalMs: 20,
        readyFramesRequired: 2,
        footprintDelayMs: 750,
        footprintToleranceMs: 25,
        windowSizePoints: [1_200, 800],
        freshRuns: freshRuns,
        warmRuns: warmRuns
    )
    let scope = ScopeReceipt(
        freshLane: "fresh_process",
        footprintOwner: "main_process_only",
        pdfGoatScope: "read_only"
    )
    try writer.append(SessionReceipt(
        record: "session",
        schemaVersion: schemaVersion,
        sessionId: session,
        startedAtUtc: ISO8601DateFormatter().string(from: Date()),
        harness: harness,
        host: startHost,
        scope: scope,
        apps: apps.map(\.receipt),
        documents: documents,
        schedule: plan
    ))
    let counts = try execute(plan, session: session, apps: apps, documents: documents, marker: manifest.marker, warmRuns: warmRuns, writer: writer)
    let endHost = try host()
    let stableHost = hostStateMatches(startHost, endHost)
    let cleanupComplete = removeSessionDirectory(temporary)
    let complete = !counts.aborted && counts.terminal == plan.count && stableHost.stable && cleanupComplete
    try writer.append(SessionEndReceipt(
        record: "session_end",
        schemaVersion: schemaVersion,
        sessionId: session,
        finishedAtUtc: ISO8601DateFormatter().string(from: Date()),
        outcome: complete ? "complete" : "aborted",
        scheduledTrials: plan.count,
        terminalReceipts: counts.terminal,
        validMeasuredTrials: counts.measured,
        failedTrials: counts.failures,
        thermalStateEnd: endHost.thermalState,
        lowPowerModeEnd: endHost.lowPowerMode,
        powerSourceUnchanged: stableHost.power,
        displayUnchanged: stableHost.display,
        lowPowerModeUnchanged: stableHost.lowPowerMode,
        thermalStateUnchanged: stableHost.thermalState,
        cleanupComplete: cleanupComplete
    ))
    try writer.close()
    if !complete { throw BenchError(code: "cleanup_failed", detail: "session aborted; raw receipts remain at \(output)") }
    print("complete session \(session): \(counts.measured) measured receipts")
}

// MARK: Summary and self-test

struct SummaryMetric: Codable {
    let n: Int
    let median: UInt64
    let medianAbsoluteDeviation: UInt64
    let p95: UInt64
    let minimum: UInt64
    let maximum: UInt64
}

struct SummaryGroup: Codable {
    let appId: String
    let documentId: String
    let lane: Lane
    let metric: String
    let latencyNs: SummaryMetric
    let physFootprintBytes: SummaryMetric
    let startupPeakBytes: SummaryMetric
}

struct SummaryReceipt: Codable {
    let schemaVersion: Int
    let rawSha256: String
    let sessionId: String
    let groups: [SummaryGroup]
}

func midpoint(_ sorted: [UInt64]) -> UInt64 {
    let upper = sorted[sorted.count / 2]
    guard sorted.count.isMultiple(of: 2) else { return upper }
    let lower = sorted[sorted.count / 2 - 1]
    return lower + (upper - lower) / 2
}

func metric(_ values: [UInt64]) throws -> SummaryMetric {
    guard !values.isEmpty else { throw UsageError(description: "empty summary metric") }
    let sorted = values.sorted()
    let median = midpoint(sorted)
    let deviations = sorted.map { $0 > median ? $0 - median : median - $0 }.sorted()
    let p95Index = (sorted.count * 95 + 99) / 100 - 1
    return SummaryMetric(
        n: sorted.count,
        median: median,
        medianAbsoluteDeviation: midpoint(deviations),
        p95: sorted[p95Index],
        minimum: sorted[0],
        maximum: sorted[sorted.count - 1]
    )
}
enum RawRecord: Decodable {
    case session(SessionReceipt)
    case trial(TrialReceipt)
    case sessionEnd(SessionEndReceipt)

    private enum Kind: String, Decodable {
        case session
        case trial
        case sessionEnd = "session_end"
    }

    private enum CodingKeys: String, CodingKey {
        case record
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        switch try values.decode(Kind.self, forKey: .record) {
        case .session:
            self = .session(try SessionReceipt(from: decoder))
        case .trial:
            self = .trial(try TrialReceipt(from: decoder))
        case .sessionEnd:
            self = .sessionEnd(try SessionEndReceipt(from: decoder))
        }
    }
}

func rawRecords(_ data: Data) throws -> [RawRecord] {
    try data.split(separator: 0x0a).filter { !$0.isEmpty }.map { line in
        do {
            return try decoder.decode(RawRecord.self, from: Data(line))
        } catch {
            throw UsageError(description: "invalid raw JSONL: \(error)")
        }
    }
}

func partitionRecords(_ records: [RawRecord]) -> (sessions: [SessionReceipt], trials: [TrialReceipt], endings: [SessionEndReceipt]) {
    var sessions: [SessionReceipt] = []
    var trials: [TrialReceipt] = []
    var endings: [SessionEndReceipt] = []
    for record in records {
        switch record {
        case let .session(value):
            sessions.append(value)
        case let .trial(value):
            trials.append(value)
        case let .sessionEnd(value):
            endings.append(value)
        }
    }
    return (sessions, trials, endings)
}

func validatedRun(_ records: [RawRecord]) throws -> (session: SessionReceipt, trials: [TrialReceipt]) {
    let parts = partitionRecords(records)
    guard parts.sessions.count == 1,
          parts.endings.count == 1,
          parts.endings[0].outcome == "complete" else {
        throw UsageError(description: "raw session is incomplete or failed")
    }
    let session = parts.sessions[0]
    let end = parts.endings[0]
    guard session.schemaVersion == schemaVersion,
          end.schemaVersion == schemaVersion,
          parts.trials.allSatisfy({ $0.schemaVersion == schemaVersion }) else {
        throw UsageError(description: "raw session has an unsupported schema version")
    }
    guard session.harness.freshRuns > 0, session.harness.warmRuns > 0 else {
        throw UsageError(description: "raw session declares invalid run counts")
    }
    let measuredTrials = parts.trials.count { $0.lane != .warmPrime }
    guard end.sessionId == session.sessionId,
          end.scheduledTrials == session.schedule.count,
          parts.trials.count == session.schedule.count,
          parts.trials.allSatisfy({ $0.status == "valid" && $0.sessionId == session.sessionId }),
          end.terminalReceipts == parts.trials.count,
          end.validMeasuredTrials == measuredTrials,
          end.failedTrials == 0 else {
        throw UsageError(description: "raw trial set is incomplete or failed")
    }
    guard end.powerSourceUnchanged,
          end.displayUnchanged,
          end.lowPowerModeUnchanged,
          end.thermalStateUnchanged,
          end.cleanupComplete,
          end.lowPowerModeEnd == session.host.lowPowerMode,
          end.thermalStateEnd == session.host.thermalState else {
        throw UsageError(description: "raw session ended with unstable host state or incomplete cleanup")
    }
    guard Set(parts.trials.map(\.sequence)).count == parts.trials.count else {
        throw UsageError(description: "raw trial sequences are missing or duplicated")
    }
    return (session, parts.trials)
}

func validateSchedule(_ schedule: [ScheduleItem], trials: [TrialReceipt]) throws {
    let bySequence = Dictionary(uniqueKeysWithValues: trials.map { ($0.sequence, $0) })
    for scheduled in schedule {
        guard let trial = bySequence[scheduled.sequence],
              trial.appId == scheduled.appId,
              trial.documentId == scheduled.documentId,
              trial.lane == scheduled.lane,
              trial.repetition == scheduled.repetition else {
            throw UsageError(description: "raw trial does not match the declared schedule")
        }
    }
}
func summaryGroups(session: SessionReceipt, trials: [TrialReceipt]) throws -> [SummaryGroup] {
    var groups: [SummaryGroup] = []
    for app in session.apps {
        for document in session.documents {
            let primeCount = trials.filter { $0.appId == app.appId && $0.documentId == document.documentId && $0.lane == .warmPrime }.count
            guard primeCount == 1 else {
                throw UsageError(description: "incomplete warm prime \(app.appId)/\(document.documentId)")
            }
            let declared = document.readiness
            for lane in [Lane.fresh, .warm] {
                let group = trials.filter { $0.appId == app.appId && $0.documentId == document.documentId && $0.lane == lane }
                let expected = lane == .fresh ? session.harness.freshRuns : session.harness.warmRuns
                guard group.count == expected else {
                    throw UsageError(description: "incomplete group \(app.appId)/\(document.documentId)/\(lane.rawValue)")
                }
                let readiness = group.compactMap(\.readiness)
                let footprints = group.compactMap(\.footprint)
                guard readiness.count == group.count, footprints.count == group.count else {
                    throw UsageError(description: "missing metric receipt")
                }
                guard readiness.allSatisfy({ $0.detector == declared }) else {
                    throw UsageError(description: "trial detector does not match the declared readiness of \(document.documentId)")
                }
                groups.append(SummaryGroup(
                    appId: app.appId,
                    documentId: document.documentId,
                    lane: lane,
                    metric: declared.metric,
                    latencyNs: try metric(readiness.map(\.latencyNs)),
                    physFootprintBytes: try metric(footprints.map(\.bytes)),
                    startupPeakBytes: try metric(footprints.map(\.startupPeakBytes))
                ))
            }
        }
    }
    return groups
}

func summary(_ raw: Data) throws -> Data {
    let run = try validatedRun(rawRecords(raw))
    try validateSchedule(run.session.schedule, trials: run.trials)
    let groups = try summaryGroups(session: run.session, trials: run.trials)
    var output = try jsonData(SummaryReceipt(schemaVersion: schemaVersion, rawSha256: digest(raw), sessionId: run.session.sessionId, groups: groups), pretty: true)
    output.append(0x0a)
    return output
}

func summarizeMode(input: String, output: String) throws {
    let inputURL = canonical(input)
    let outputURL = canonical(output)
    guard inputURL != outputURL else {
        throw UsageError(description: "summary output must differ from raw input")
    }
    let raw = try Data(contentsOf: inputURL)
    let data = try summary(raw)
    try FileManager.default.createDirectory(at: outputURL.deletingLastPathComponent(), withIntermediateDirectories: true)
    try data.write(to: outputURL, options: .atomic)
    print("wrote \(outputURL.path) from raw SHA-256 \(digest(raw))")
}

func synthetic(marker: Marker, correct: Bool) -> Pixels {
    let width = 160, height = 120
    var bytes = [UInt8](repeating: 255, count: width * height * 4)
    func fill(_ x: Int, _ y: Int, _ color: [Int]) {
        for row in y..<(y + 18) { for column in x..<(x + 18) { let offset = (row * width + column) * 4; bytes[offset] = UInt8(color[0]); bytes[offset + 1] = UInt8(color[1]); bytes[offset + 2] = UInt8(color[2]); bytes[offset + 3] = 255 } }
    }
    fill(30, 24, marker.red); fill(48, 24, correct ? marker.green : marker.blue); fill(30, 42, correct ? marker.blue : marker.green); fill(48, 42, marker.yellow)
    return Pixels(width: width, height: height, bytes: bytes)
}

// Frames the content detector has to judge: white paper carrying black ink, the blank white
// window Preview showed on dive, and the grey window it showed on pst-geo with restored scroll
// state. `paper` bounds the white area (the whole frame by default) and `ink` the black area.
// Alpha stays opaque, as it was in those captures.
typealias FrameBox = (x: Range<Int>, y: Range<Int>)
let inkStripe: FrameBox = (x: 80..<200, y: 60..<70)

func syntheticFrame(background: UInt8, ink: FrameBox?, paper: FrameBox? = nil) -> Pixels {
    let width = 240, height = 160
    var bytes = [UInt8](repeating: background, count: width * height * 4)
    for offset in stride(from: 3, to: bytes.count, by: 4) { bytes[offset] = 255 }
    func paint(_ value: UInt8, _ box: FrameBox) {
        for row in box.y {
            for column in box.x {
                let offset = (row * width + column) * 4
                bytes[offset] = value; bytes[offset + 1] = value; bytes[offset + 2] = value
            }
        }
    }
    if let paper { paint(255, paper) }
    if let ink { paint(0, ink) }
    return Pixels(width: width, height: height, bytes: bytes)
}

func check(_ condition: @autoclosure () -> Bool, _ message: String) throws {
    if !condition() { throw UsageError(description: "self-test failed: \(message)") }
}

func selfTest() throws {
    let manifest = try loadManifest(), files = generated(manifest)
    func orientedImage(_ pixels: Pixels) -> CGImage? {
        guard let provider = CGDataProvider(data: Data(pixels.bytes) as CFData) else { return nil }
        return CGImage(
            width: pixels.width,
            height: pixels.height,
            bitsPerComponent: 8,
            bitsPerPixel: 32,
            bytesPerRow: pixels.width * 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGBitmapInfo(rawValue: CGBitmapInfo.byteOrder32Big.rawValue | CGImageAlphaInfo.premultipliedLast.rawValue),
            provider: provider,
            decode: nil,
            shouldInterpolate: false,
            intent: .defaultIntent
        )
    }

    for fixture in manifest.documents {
        let data = files[fixture.fileName]!
        print("fixture \(fixture.fileName) sha256=\(digest(data)) bytes=\(data.count)")
    }
    _ = try checkedCorpus(manifest)
    try check(markerScore(synthetic(marker: manifest.marker, correct: true), marker: manifest.marker) != nil, "valid marker")
    try check(markerScore(synthetic(marker: manifest.marker, correct: false), marker: manifest.marker) == nil, "wrong adjacency")
    guard let oriented = orientedImage(synthetic(marker: manifest.marker, correct: true)),
          let converted = rgbaPixels(from: oriented) else {
        throw UsageError(description: "self-test failed: capture conversion")
    }
    try check(markerScore(converted, marker: manifest.marker) != nil, "capture orientation")
    let inked = inkScore(syntheticFrame(background: 255, ink: inkStripe))
    try check(inked.passes && inked.stage == "measured", "content detector on paper and ink")
    try check(frameScore(syntheticFrame(background: 255, ink: inkStripe), readiness: .content, marker: manifest.marker).score != nil, "content frame score")
    let blank = inkScore(syntheticFrame(background: 255, ink: nil))
    try check(!blank.passes && blank.stage == "measured" && blank.inkPixels == 0, "content detector on blank paper")
    let grey = inkScore(syntheticFrame(background: 128, ink: nil))
    try check(!grey.passes && grey.stage == "no_paper_span" && grey.alphaMax == 255, "content detector on a grey window")
    // A white patch too small to span a quarter of any row, then a white-bordered page whose
    // interior is ink.
    let patch = inkScore(syntheticFrame(background: 0, ink: (x: 105..<115, y: 62..<68), paper: (x: 100..<120, y: 55..<75)))
    try check(!patch.passes && patch.stage == "no_paper_span" && patch.qualifyingRows == 0, "content detector on a small white patch")
    let darkPage = inkScore(syntheticFrame(background: 255, ink: (x: 80..<210, y: 32..<138)))
    try check(!darkPage.passes && darkPage.stage == "measured" && darkPage.paperFraction < minimumPaperFraction, "content detector on a mostly dark page")
    var tracker = Tracker()
    try check(tracker.add(time: 100, score: 0.9) == nil, "one frame")
    _ = tracker.add(time: 120, score: nil)
    _ = tracker.add(time: 140, score: 0.8)
    let ready = tracker.add(time: 160, score: 0.85)
    try check(ready?.0 == 140 && ready?.1 == 160, "readiness bounce")
    var painting = Tracker()
    _ = painting.add(time: 100, score: 0.088)
    try check(painting.add(time: 120, score: 0.161) == nil, "a growing score restarts the candidate")
    let steady = painting.add(time: 140, score: 0.165)
    try check(steady?.0 == 120 && steady?.1 == 140, "a steady score confirms")
    let arithmetic = try metric([100, 110, 120, 130, 900])
    try check(
        arithmetic.median == 120 &&
            arithmetic.medianAbsoluteDeviation == 10 &&
            arithmetic.p95 == 900,
        "summary arithmetic"
    )
    let evenArithmetic = try metric([100, 110])
    try check(
        evenArithmetic.median == 105 &&
            evenArithmetic.medianAbsoluteDeviation == 5 &&
            evenArithmetic.p95 == 110,
        "even summary arithmetic"
    )
    try check(footprintDelayNS + footprintToleranceNS == 775_000_000 && 776_000_000 > footprintDelayNS + footprintToleranceNS, "sample deadline")

    func testApp(_ id: String, role: String = "competitor") -> App {
        let receipt = AppReceipt(
            appId: id,
            role: role,
            bundlePath: "/\(id).app",
            bundleId: "test.\(id)",
            shortVersion: "1",
            bundleVersion: "1",
            infoPlistSha256: "info",
            executablePath: "/\(id).app/executable",
            executableSha256: "executable",
            architecture: "arm64",
            signingId: id,
            teamId: nil,
            cdhash: "cdhash"
        )
        return App(id: id, role: role, bundle: receipt.bundlePath, executable: receipt.executablePath, executableHash: receipt.executableSha256, infoHash: receipt.infoPlistSha256, receipt: receipt)
    }

    func testDocument(_ id: String, readiness: Readiness = .marker) -> DocumentReceipt {
        DocumentReceipt(documentId: id, path: "/\(id).pdf", sha256: "pdf", bytes: 1, pages: 1, readiness: readiness, generatorSchema: 1, markerSchema: 1)
    }

    func syntheticSummaryRaw(
        app: App,
        document: DocumentReceipt,
        freshRuns: Int = 5,
        warmRuns: Int = 3,
        receiptSchema: Int = schemaVersion,
        cleanupComplete: Bool = true,
        hostStable: Bool = true,
        declaredScheduledTrials: Int? = nil,
        detector: Readiness? = nil
    ) throws -> Data {
        let plan = schedule(
            apps: [app],
            documents: [document],
            freshRuns: freshRuns,
            warmRuns: warmRuns
        )
        let host = HostReceipt(
            hardwareModel: "test",
            cpu: "test",
            memoryBytes: 1,
            osVersion: "test",
            osBuild: "test",
            displayPoints: [1_280, 900],
            displayPixels: [1_280, 900],
            backingScale: 1,
            refreshHz: 60,
            colorSpace: "test",
            powerSource: "ac",
            lowPowerMode: false,
            thermalState: "nominal",
            diskCachePurged: false
        )
        let harness = HarnessReceipt(
            scriptSha256: "script",
            argv: [],
            pollIntervalMs: 20,
            readyFramesRequired: 2,
            footprintDelayMs: 750,
            footprintToleranceMs: 25,
            windowSizePoints: [1_200, 800],
            freshRuns: freshRuns,
            warmRuns: warmRuns
        )
        let scope = ScopeReceipt(
            freshLane: "fresh_process",
            footprintOwner: "main_process_only",
            pdfGoatScope: "read_only"
        )
        var raw = Data()
        try raw.append(jsonLine(SessionReceipt(
            record: "session",
            schemaVersion: receiptSchema,
            sessionId: "synthetic",
            startedAtUtc: "2026-09-01T00:00:00Z",
            harness: harness,
            host: host,
            scope: scope,
            apps: [app.receipt],
            documents: [document],
            schedule: plan
        )))
        for item in plan {
            let latency: UInt64
            switch item.lane {
            case .fresh:
                latency = UInt64(90 + item.repetition * 10)
            case .warm:
                latency = UInt64(190 + item.repetition * 10)
            case .warmPrime:
                latency = 0
            }
            var trial = baseReceipt(session: "synthetic", item: item, kind: "synthetic")
            trial.readiness = ReadinessReceipt(
                detector: detector ?? document.readiness,
                framesExamined: 2,
                tFirstPassNs: latency,
                tConfirmNs: latency + 20,
                latencyNs: latency + 20,
                firstMatchScore: 1,
                confirmMatchScore: 1
            )
            trial.footprint = FootprintReceipt(
                api: "synthetic",
                atReadinessBytes: latency,
                dueNs: latency + 770,
                actualNs: latency + 770,
                offsetFromReadyNs: 750,
                latenessNs: 0,
                bytes: latency + 1_000,
                startupPeakBytes: latency + 2_000,
                timeSeries: []
            )
            valid(&trial)
            try raw.append(jsonLine(trial))
        }
        try raw.append(jsonLine(SessionEndReceipt(
            record: "session_end",
            schemaVersion: receiptSchema,
            sessionId: "synthetic",
            finishedAtUtc: "2026-09-01T00:00:01Z",
            outcome: "complete",
            scheduledTrials: declaredScheduledTrials ?? plan.count,
            terminalReceipts: plan.count,
            validMeasuredTrials: freshRuns + warmRuns,
            failedTrials: 0,
            thermalStateEnd: "nominal",
            lowPowerModeEnd: false,
            powerSourceUnchanged: hostStable,
            displayUnchanged: hostStable,
            lowPowerModeUnchanged: hostStable,
            thermalStateUnchanged: hostStable,
            cleanupComplete: cleanupComplete
        )))
        return raw
    }

    func expectUsageError(_ message: String, _ operation: () throws -> Void) throws {
        do {
            try operation()
        } catch is UsageError {
            return
        }
        throw UsageError(description: "self-test failed: \(message)")
    }

    func expectBenchError(_ code: String, _ message: String, _ operation: () throws -> Void) throws {
        do {
            try operation()
        } catch let error as BenchError {
            try check(error.code == code, "\(message): got \(error.code)")
            return
        }
        throw UsageError(description: "self-test failed: \(message)")
    }

    let fake = (0..<4).map { testApp("a\($0)") }
    let docs = [testDocument("tiny"), testDocument("mixed")]
    let plan = schedule(apps: fake, documents: docs, freshRuns: 5, warmRuns: 3)
    try check(plan.count { $0.lane == .fresh } == 40, "fresh count")
    try check(plan.count { $0.lane == .warm } == 24, "warm count")
    try check(plan.count { $0.lane == .warmPrime } == 8, "prime count")
    try expectUsageError("incomplete summary passed") {
        _ = try summary(Data("{}\n".utf8))
    }

    let summaryApp = testApp("subject", role: "subject")
    let summaryDocument = testDocument("tiny")
    let raw = try syntheticSummaryRaw(app: summaryApp, document: summaryDocument)
    let summaryA = try summary(raw)
    let summaryB = try summary(raw)
    try check(summaryA == summaryB && digest(summaryA) == digest(summaryB), "repeat summary")
    let summaryObject = try decoder.decode(SummaryReceipt.self, from: summaryA)
    try check(summaryObject.schemaVersion == schemaVersion, "summary schema")
    try check(summaryObject.rawSha256 == digest(raw), "summary raw hash")
    try check(summaryObject.groups[0].latencyNs.p95 == 160, "summary p95")
    try check(summaryObject.groups[0].metric == "request_to_confirmed_visible_marker", "summary metric")

    let contentDocument = testDocument("real", readiness: .content)
    let contentSummary = try decoder.decode(
        SummaryReceipt.self,
        from: summary(try syntheticSummaryRaw(app: summaryApp, document: contentDocument, freshRuns: 1, warmRuns: 1))
    )
    try check(contentSummary.groups.allSatisfy { $0.metric == "request_to_confirmed_visible_content" }, "content metric in summary")
    try expectUsageError("detector unlike the declared readiness passed") {
        let invalid = try syntheticSummaryRaw(
            app: summaryApp,
            document: contentDocument,
            detector: .marker
        )
        _ = try summary(invalid)
    }

    let shortRaw = try syntheticSummaryRaw(
        app: summaryApp,
        document: summaryDocument,
        freshRuns: 1,
        warmRuns: 1
    )
    let shortSummary = try decoder.decode(SummaryReceipt.self, from: summary(shortRaw))
    try check(shortSummary.groups.count == 2, "declared run counts")

    try expectUsageError("unsupported receipt schema passed") {
        let invalid = try syntheticSummaryRaw(
            app: summaryApp,
            document: summaryDocument,
            receiptSchema: schemaVersion - 1
        )
        _ = try summary(invalid)
    }
    try expectUsageError("incomplete cleanup passed") {
        let invalid = try syntheticSummaryRaw(
            app: summaryApp,
            document: summaryDocument,
            cleanupComplete: false
        )
        _ = try summary(invalid)
    }
    try expectUsageError("unstable host passed") {
        let invalid = try syntheticSummaryRaw(
            app: summaryApp,
            document: summaryDocument,
            hostStable: false
        )
        _ = try summary(invalid)
    }
    try expectUsageError("declared trial count mismatch passed") {
        let invalid = try syntheticSummaryRaw(
            app: summaryApp,
            document: summaryDocument,
            declaredScheduledTrials: 0
        )
        _ = try summary(invalid)
    }

    let generateDirectory = FileManager.default.temporaryDirectory
        .appendingPathComponent("pdf-goat-generate-\(UUID().uuidString)")
    try FileManager.default.createDirectory(at: generateDirectory, withIntermediateDirectories: true)
    defer { try? FileManager.default.removeItem(at: generateDirectory) }
    let protectedOutput = generateDirectory.appendingPathComponent(manifest.documents[0].fileName)
    let sentinel = Data("preserve existing output".utf8)
    try sentinel.write(to: protectedOutput)
    try expectBenchError("output_exists", "generate replaced an existing fixture") {
        try generateMode(output: generateDirectory.path)
    }
    let preservedOutput = try Data(contentsOf: protectedOutput)
    try check(preservedOutput == sentinel, "generate preserved existing fixture")

    // corpus.json names no readiness, so its documents must arrive as marker documents, and an
    // external entry must be verified on disk rather than regenerated.
    let corpusDirectory = generateDirectory.appendingPathComponent("corpus")
    let sessionDirectory = generateDirectory.appendingPathComponent("session")
    for directory in [corpusDirectory, sessionDirectory] {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    }
    for fixture in manifest.documents {
        try files[fixture.fileName]!.write(to: corpusDirectory.appendingPathComponent(fixture.fileName))
    }
    let copied = try copyDocuments(manifest: manifest, corpusPath: corpusDirectory.path, destination: sessionDirectory)
    try check(copied.count == manifest.documents.count, "manifest documents copied")
    try check(copied.allSatisfy { $0.readiness == .marker }, "manifest without readiness decodes as marker")

    let externalBytes = files[manifest.documents[0].fileName]!
    try externalBytes.write(to: corpusDirectory.appendingPathComponent("external.pdf"))
    func externalManifest(sha256: String?, byteCount: Int?) -> Manifest {
        Manifest(
            generatorSchemaVersion: manifest.generatorSchemaVersion,
            marker: manifest.marker,
            documents: [Fixture(
                documentId: "external",
                fileName: "external.pdf",
                path: "external.pdf",
                sha256: sha256,
                byteCount: byteCount,
                pageCount: 3,
                readiness: .content
            )]
        )
    }
    for (label, sha256, byteCount) in [
        ("wrong digest", String(repeating: "0", count: 64), externalBytes.count),
        ("no digest", nil, externalBytes.count),
        ("no byte count", digest(externalBytes), nil),
    ] as [(String, String?, Int?)] {
        try expectBenchError("corpus_mismatch", "external entry with \(label) passed") {
            _ = try copyDocuments(
                manifest: externalManifest(sha256: sha256, byteCount: byteCount),
                corpusPath: corpusDirectory.path,
                destination: generateDirectory.appendingPathComponent("rejected-\(label)")
            )
        }
    }
    let honest = externalManifest(sha256: digest(externalBytes), byteCount: externalBytes.count)
    _ = try checkedCorpus(honest)  // an external entry is not regenerated or digest-checked here
    let external = try copyDocuments(manifest: honest, corpusPath: corpusDirectory.path, destination: sessionDirectory)
    try check(
        external.count == 1 && external[0].readiness == .content && external[0].bytes == externalBytes.count,
        "external entry verified and copied"
    )

    let aliasURL = FileManager.default.temporaryDirectory
        .appendingPathComponent("pdf-goat-summary-alias-\(UUID().uuidString).jsonl")
    try raw.write(to: aliasURL)
    defer { try? FileManager.default.removeItem(at: aliasURL) }
    try expectUsageError("summary replaced raw input") {
        try summarizeMode(input: aliasURL.path, output: aliasURL.path)
    }
    let preservedRaw = try Data(contentsOf: aliasURL)
    try check(preservedRaw == raw, "summary input preserved")
    print("self-test: passed; no app launched")
}

// MARK: CLI

let usage = """
usage:
  swift benchmarks/pdf_benchmark.swift generate --output DIR
  swift benchmarks/pdf_benchmark.swift self-test
  swift benchmarks/pdf_benchmark.swift run --corpus DIR --output RAW.jsonl --pdf-goat APP [--preview APP] [--pdfgear APP] [--skim APP] [--fresh-runs 5] [--warm-runs 3]
  swift benchmarks/pdf_benchmark.swift summarize RAW.jsonl --output SUMMARY.json
"""

func option(_ flag: String, _ arguments: [String]) throws -> String? {
    let indexes = arguments.indices.filter { arguments[$0] == flag }
    guard indexes.count <= 1 else { throw UsageError(description: "duplicate option \(flag)") }
    guard let index = indexes.first else { return nil }
    guard index + 1 < arguments.count, !arguments[index + 1].hasPrefix("--") else { throw UsageError(description: "missing value for \(flag)") }
    return arguments[index + 1]
}

func required(_ flag: String, _ arguments: [String]) throws -> String {
    guard let value = try option(flag, arguments) else { throw UsageError(description: "missing \(flag)\n\(usage)") }
    return value
}

func positive(_ flag: String, _ arguments: [String], default value: Int) throws -> Int {
    guard let text = try option(flag, arguments) else { return value }
    guard let number = Int(text), number > 0 else { throw UsageError(description: "\(flag) must be positive") }
    return number
}

func rejectUnknown(_ arguments: [String], allowed: Set<String>) throws {
    var index = 0
    while index < arguments.count {
        guard allowed.contains(arguments[index]), index + 1 < arguments.count else { throw UsageError(description: "unknown or incomplete option \(arguments[index])") }
        index += 2
    }
}

func main() throws {
    let arguments = Array(CommandLine.arguments.dropFirst())
    guard let mode = arguments.first else { throw UsageError(description: usage) }
    let rest = Array(arguments.dropFirst())
    switch mode {
    case "generate":
        try rejectUnknown(rest, allowed: ["--output"])
        try generateMode(output: required("--output", rest))
    case "self-test":
        guard rest.isEmpty else { throw UsageError(description: usage) }
        try selfTest()
    case "run":
        try rejectUnknown(rest, allowed: ["--corpus", "--output", "--pdf-goat", "--preview", "--pdfgear", "--skim", "--fresh-runs", "--warm-runs"])
        try runMode(arguments: rest)
    case "summarize":
        guard let input = rest.first, !input.hasPrefix("--") else { throw UsageError(description: usage) }
        let options = Array(rest.dropFirst())
        try rejectUnknown(options, allowed: ["--output"])
        try summarizeMode(input: input, output: required("--output", options))
    case "help", "--help", "-h": print(usage)
    default: throw UsageError(description: usage)
    }
}

do {
    try main()
} catch {
    FileHandle.standardError.write(Data("\(error)\n".utf8))
    exit(error is UsageError ? 64 : 1)
}
