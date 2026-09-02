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

    /// A shown controller over blank pages, one main-queue turn after its
    /// first frame. The caller closes it.
    private func shownController(pages: [PDFPage]) -> (DocumentWindowController, PDFDocument) {
        let source = PDFDocument()
        for (index, page) in pages.enumerated() {
            source.insert(page, at: index)
        }
        let controller = DocumentWindowController(document: source)
        controller.showWindow(nil)
        RunLoop.current.run(until: Date().addingTimeInterval(0.1))
        return (controller, source)
    }

    private func pdfView(of controller: DocumentWindowController) throws -> PDFView {
        let contentView = try XCTUnwrap(controller.window?.contentView)
        return try XCTUnwrap(firstSubview(of: PDFView.self, in: contentView))
    }

    func testPageNavigationShowsNextPage() throws {
        let (controller, source) = shownController(pages: [PDFPage(), PDFPage()])
        defer { controller.close() }
        let pdfView = try pdfView(of: controller)
        XCTAssertEqual(pdfView.visiblePages.map { source.index(for: $0) }, [0])

        controller.nextPage(nil)
        RunLoop.current.run(until: Date().addingTimeInterval(0.5))

        XCTAssertEqual(pdfView.visiblePages.map { source.index(for: $0) }, [1])
    }

    func testOpeningShowsDocumentStart() throws {
        let (controller, source) = shownController(pages: [PDFPage(), PDFPage()])
        defer { controller.close() }
        let pdfView = try pdfView(of: controller)
        let documentView = try XCTUnwrap(pdfView.documentView)
        let first = try XCTUnwrap(source.page(at: 0))
        let firstInView = pdfView.convert(first.bounds(for: .cropBox), from: first)

        XCTAssertGreaterThan(documentView.bounds.height, documentView.visibleRect.height)
        XCTAssertTrue(pdfView.bounds.contains(NSPoint(x: firstInView.midX, y: firstInView.maxY)))
    }

    func testOpeningShowsARotatedFirstPageFromItsTop() throws {
        for rotation in [90, 180, 270] {
            let rotated = PDFPage()
            rotated.rotation = rotation
            let (controller, source) = shownController(pages: [rotated, PDFPage(), PDFPage()])
            defer { controller.close() }
            let pdfView = try pdfView(of: controller)

            XCTAssertEqual(pdfView.visiblePages.map { source.index(for: $0) }, [0], "rotation \(rotation)")
            let firstInView = pdfView.convert(rotated.bounds(for: .cropBox), from: rotated)
            XCTAssertTrue(pdfView.bounds.contains(NSPoint(x: firstInView.midX, y: firstInView.maxY)), "rotation \(rotation)")
        }
    }

    func testLiveScrollLowersInterpolationQuality() throws {
        let (controller, _) = shownController(pages: [PDFPage(), PDFPage()])
        defer { controller.close() }
        let pdfView = try pdfView(of: controller)
        let scrollView = try XCTUnwrap(pdfView.documentView?.enclosingScrollView)
        XCTAssertEqual(pdfView.interpolationQuality, .high)

        NotificationCenter.default.post(name: NSScrollView.willStartLiveScrollNotification, object: scrollView)
        XCTAssertEqual(pdfView.interpolationQuality, .low)

        NotificationCenter.default.post(name: NSScrollView.didEndLiveScrollNotification, object: scrollView)
        XCTAssertEqual(pdfView.interpolationQuality, .high)
    }

    func testThumbnailSidebarLinksAfterFirstContent() throws {
        let source = PDFDocument()
        source.insert(PDFPage(), at: 0)
        let controller = DocumentWindowController(document: source)
        controller.showWindow(nil)
        defer { controller.close() }

        let contentView = try XCTUnwrap(controller.window?.contentView)
        XCTAssertNil(firstSubview(of: PDFThumbnailView.self, in: contentView))

        RunLoop.current.run(until: Date().addingTimeInterval(0.1))

        let thumbnails = try XCTUnwrap(firstSubview(of: PDFThumbnailView.self, in: contentView))
        XCTAssertIdentical(thumbnails.pdfView, firstSubview(of: PDFView.self, in: contentView))
    }

    func testWidgetAddedAfterFirstVisiblePassBecomesReadOnly() throws {
        let page = PDFPage()
        let (controller, _) = shownController(pages: [page])
        defer { controller.close() }

        let widget = PDFAnnotation(
            bounds: NSRect(x: 40, y: 40, width: 180, height: 24),
            forType: .widget,
            withProperties: nil
        )
        widget.widgetFieldType = .text
        page.addAnnotation(widget)
        XCTAssertFalse(widget.isReadOnly)

        NotificationCenter.default.post(name: .PDFViewVisiblePagesChanged, object: try pdfView(of: controller))

        XCTAssertTrue(widget.isReadOnly)
    }

    func testThumbnailSidebarReleasesUnderPressureAndRelinksAfter() throws {
        let (controller, _) = shownController(pages: [PDFPage()])
        defer { controller.close() }
        let contentView = try XCTUnwrap(controller.window?.contentView)
        let linked = try XCTUnwrap(firstSubview(of: PDFThumbnailView.self, in: contentView))

        controller.releaseThumbnailSidebar()
        RunLoop.current.run(until: Date().addingTimeInterval(0.1))
        XCTAssertNil(firstSubview(of: PDFThumbnailView.self, in: contentView))

        controller.linkThumbnailSidebar()
        controller.linkThumbnailSidebar()
        let rebuilt = try XCTUnwrap(firstSubview(of: PDFThumbnailView.self, in: contentView))
        XCTAssertNotIdentical(rebuilt, linked)
        XCTAssertIdentical(rebuilt.pdfView, firstSubview(of: PDFView.self, in: contentView))
        XCTAssertEqual(thumbnailViewCount(in: contentView), 1)
    }

    private func thumbnailViewCount(in view: NSView) -> Int {
        (view is PDFThumbnailView ? 1 : 0) + view.subviews.map { thumbnailViewCount(in: $0) }.reduce(0, +)
    }
}
