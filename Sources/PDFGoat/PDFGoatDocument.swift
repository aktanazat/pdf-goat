import AppKit
import PDFKit

@MainActor
final class PDFGoatDocument: NSDocument {
    let pdfDocument: PDFDocument

    init(sourceURL: URL) throws {
        let normalizedURL = sourceURL.standardizedFileURL.resolvingSymlinksInPath()
        guard sourceURL.isFileURL, let pdfDocument = PDFDocument(url: normalizedURL) else {
            throw DocumentOpenError.unreadable(sourceURL.lastPathComponent)
        }

        self.pdfDocument = pdfDocument
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
