import AppKit
import OSLog
import PDFKit

/// Launch signposts, read with
/// `log show --last 2m --signpost --predicate 'subsystem == "dev.aktan.pdfgoat"'`.
enum LaunchTrace {
    static let signposter = OSSignposter(subsystem: "dev.aktan.pdfgoat", category: "launch")
}

@MainActor
final class PDFGoatDocument: NSDocument {
    let pdfDocument: PDFDocument

    init(sourceURL: URL) throws {
        let normalizedURL = sourceURL.standardizedFileURL.resolvingSymlinksInPath()
        let opened = LaunchTrace.signposter.withIntervalSignpost("open.document") {
            sourceURL.isFileURL ? PDFDocument(url: normalizedURL) : nil
        }
        guard let opened else {
            throw DocumentOpenError.unreadable(sourceURL.lastPathComponent)
        }

        pdfDocument = opened
        super.init()
        fileURL = normalizedURL
    }

    override func makeWindowControllers() {
        addWindowController(DocumentWindowController(document: pdfDocument))
    }
}

enum DocumentOpenError: LocalizedError {
    case unreadable(String)

    var errorDescription: String? {
        switch self {
        case let .unreadable(name):
            "PDF Goat could not open \(name)."
        }
    }
}
