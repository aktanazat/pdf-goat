import Foundation
import PDFKit
import XCTest
@testable import PDFGoat

@MainActor
final class PDFGoatDocumentTests: XCTestCase {
    private func firstSubview<T: NSView>(of type: T.Type, in view: NSView) -> T? {
        if let match = view as? T {
            return match
        }
        return view.subviews.lazy.compactMap { self.firstSubview(of: type, in: $0) }.first
    }

    func testOpensLocalPDF() throws {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
            .appendingPathExtension("pdf")
        defer { try? FileManager.default.removeItem(at: url) }

        let source = PDFDocument()
        source.insert(PDFPage(), at: 0)
        XCTAssertTrue(source.write(to: url))

        let document = try PDFGoatDocument(sourceURL: url)

        XCTAssertEqual(document.pdfDocument.pageCount, 1)
        XCTAssertEqual(document.fileURL, url.standardizedFileURL.resolvingSymlinksInPath())
    }

    func testRejectsMissingPDF() {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
            .appendingPathExtension("pdf")

        XCTAssertThrowsError(try PDFGoatDocument(sourceURL: url)) { error in
            XCTAssertEqual(
                error.localizedDescription,
                "PDF Goat could not open " + url.lastPathComponent + "."
            )
        }
    }

    func testExternalLinkDisplaysBlockedAlert() throws {
        let source = PDFDocument()
        source.insert(PDFPage(), at: 0)
        let controller = DocumentWindowController(document: source)
        controller.showWindow(nil)
        defer { controller.close() }

        XCTAssertTrue(
            controller.responds(
                to: NSSelectorFromString("PDFViewWillClickOnLink:withURL:")
            )
        )
        let contentView = try XCTUnwrap(controller.window?.contentView)
        let pdfView = try XCTUnwrap(firstSubview(of: PDFView.self, in: contentView))
        XCTAssertTrue(pdfView.delegate === controller)

        controller.pdfViewWillClick(
            onLink: pdfView,
            with: try XCTUnwrap(URL(string: "https://example.com"))
        )
        RunLoop.current.run(until: Date().addingTimeInterval(0.1))

        XCTAssertNotNil(controller.window?.attachedSheet)
    }

    func testFormWidgetsAreReadOnly() {
        let source = PDFDocument()
        let page = PDFPage()
        let widget = PDFAnnotation(
            bounds: NSRect(x: 40, y: 40, width: 180, height: 24),
            forType: .widget,
            withProperties: nil
        )
        widget.widgetFieldType = .text
        let link = PDFAnnotation(
            bounds: NSRect(x: 40, y: 80, width: 180, height: 24),
            forType: .link,
            withProperties: nil
        )
        page.addAnnotation(widget)
        page.addAnnotation(link)
        source.insert(page, at: 0)
        XCTAssertFalse(widget.isReadOnly)
        XCTAssertFalse(link.isReadOnly)

        let controller = DocumentWindowController(document: source)
        controller.showWindow(nil)
        defer { controller.close() }
        RunLoop.current.run(until: Date().addingTimeInterval(0.1))

        XCTAssertTrue(widget.isReadOnly)
        XCTAssertFalse(link.isReadOnly)
    }

    func testPageNavigationUpdatesCurrentPage() throws {
        let source = PDFDocument()
        source.insert(PDFPage(), at: 0)
        source.insert(PDFPage(), at: 1)
        let controller = DocumentWindowController(document: source)
        controller.showWindow(nil)
        defer { controller.close() }

        let contentView = try XCTUnwrap(controller.window?.contentView)
        let pdfView = try XCTUnwrap(firstSubview(of: PDFView.self, in: contentView))
        XCTAssertEqual(source.index(for: try XCTUnwrap(pdfView.currentPage)), 0)

        controller.nextPage(nil)
        RunLoop.current.run(until: Date().addingTimeInterval(0.5))

        XCTAssertEqual(source.index(for: try XCTUnwrap(pdfView.currentPage)), 1)
    }

}
