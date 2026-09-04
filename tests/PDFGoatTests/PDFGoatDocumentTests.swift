import AppKit
import Foundation
import PDFKit
import Testing
@testable import PDFGoat

@Suite("PDF Goat document behavior", .serialized)
@MainActor
struct PDFGoatDocumentTests {
    @Test("A local PDF opens with its content and normalized source URL")
    func localPDFOpens() throws {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
            .appendingPathExtension("pdf")
        defer { try? FileManager.default.removeItem(at: url) }

        let source = PDFDocument()
        source.insert(PDFPage(), at: 0)
        try #require(source.write(to: url))

        let document = try PDFGoatDocument(sourceURL: url)

        #expect(document.pdfDocument.pageCount == 1)
        #expect(document.fileURL == url.standardizedFileURL.resolvingSymlinksInPath())
    }

    @Test("A missing local PDF reports the unreadable file")
    func missingPDFIsRejected() {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString)
            .appendingPathExtension("pdf")

        let error = #expect(throws: DocumentOpenError.self) {
            try PDFGoatDocument(sourceURL: url)
        }

        #expect(error?.localizedDescription == "PDF Goat could not open " + url.lastPathComponent + ".")
    }

    @Test("Clicking an external link presents a blocked-link sheet")
    func externalLinksAreBlocked() throws {
        let (controller, _) = shownController(pages: [PDFPage()])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        let link = try #require(URL(string: "https://example.com"))

        controller.pdfViewWillClick(onLink: pdfView, with: link)

        #expect(waitFor(true) { controller.window?.attachedSheet != nil } == true)
    }

    @Test("Visible form widgets become read-only without changing links")
    func visibleWidgetsAreReadOnly() {
        let page = PDFPage()
        let widget = formWidget()
        let link = PDFAnnotation(
            bounds: NSRect(x: 40, y: 80, width: 180, height: 24),
            forType: .link,
            withProperties: nil
        )
        page.addAnnotation(widget)
        page.addAnnotation(link)

        let (controller, _) = shownController(pages: [page])
        defer { controller.close() }

        #expect(waitFor(true) { widget.isReadOnly } == true)
        #expect(link.isReadOnly == false)
    }

    @Test("A widget added after opening becomes read-only when its page appears")
    func lateWidgetsAreReadOnly() throws {
        let firstPage = PDFPage()
        let secondPage = PDFPage()
        let (controller, document) = shownController(pages: [firstPage, secondPage])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])

        let widget = formWidget()
        secondPage.addAnnotation(widget)
        pdfView.go(to: secondPage)

        #expect(waitFor(true) { widget.isReadOnly } == true)
    }

    @Test("Opening positions the first page at its top", arguments: [0, 90, 180, 270])
    func openingPositionsFirstPageAtTop(rotation: Int) throws {
        let firstPage = PDFPage()
        firstPage.rotation = rotation
        let (controller, document) = shownController(pages: [firstPage, PDFPage(), PDFPage()])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))

        #expect(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let firstPageBounds = pdfView.convert(firstPage.bounds(for: .cropBox), from: firstPage)
        #expect(pdfView.bounds.contains(NSPoint(x: firstPageBounds.midX, y: firstPageBounds.maxY)))
    }

    @Test("Next page moves the visible page forward")
    func nextPageMovesForward() throws {
        let (controller, document) = shownController(pages: [PDFPage(), PDFPage()])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])

        controller.nextPage(nil)

        #expect(waitFor([1]) { visiblePageIndexes(of: pdfView, in: document) } == [1])
    }

    @Test("Live scrolling uses low interpolation and restores high after it ends")
    func liveScrollAdjustsInterpolationQuality() async throws {
        let (controller, _) = shownController(pages: [PDFPage(), PDFPage()])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        NotificationCenter.default.post(name: .PDFViewVisiblePagesChanged, object: pdfView)
        await nextMainQueueTurn()
        let scrollView = try #require(pdfView.documentView?.enclosingScrollView)

        #expect(pdfView.interpolationQuality == .high)
        NotificationCenter.default.post(
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        #expect(pdfView.interpolationQuality == .low)
        NotificationCenter.default.post(
            name: NSScrollView.didEndLiveScrollNotification,
            object: scrollView
        )
        #expect(pdfView.interpolationQuality == .high)
    }

    @Test("Thumbnail sidebar releases under pressure and relinks to the document")
    func thumbnailSidebarReleasesAndRelinks() throws {
        let (controller, _) = shownController(pages: [PDFPage()])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        controller.linkThumbnailSidebar()
        _ = try #require(thumbnailView(of: controller))

        controller.releaseThumbnailSidebar()
        #expect(thumbnailView(of: controller) == nil)

        controller.linkThumbnailSidebar()

        let thumbnails = try #require(thumbnailView(of: controller))
        let linkedPDFView = try #require(thumbnails.pdfView)
        #expect(linkedPDFView === pdfView)
    }

    private func nextMainQueueTurn() async {
        await withCheckedContinuation { continuation in
            DispatchQueue.main.async {
                continuation.resume()
            }
        }
    }

    private func shownController(pages: [PDFPage]) -> (DocumentWindowController, PDFDocument) {
        let document = PDFDocument()
        for (index, page) in pages.enumerated() {
            document.insert(page, at: index)
        }
        let controller = DocumentWindowController(document: document)
        controller.showWindow(nil)
        controller.window?.displayIfNeeded()
        return (controller, document)
    }

    private func formWidget() -> PDFAnnotation {
        let widget = PDFAnnotation(
            bounds: NSRect(x: 40, y: 40, width: 180, height: 24),
            forType: .widget,
            withProperties: nil
        )
        widget.widgetFieldType = .text
        return widget
    }

    private func displayedPDFView(of controller: DocumentWindowController) -> PDFView? {
        splitViewController(of: controller)?
            .splitViewItems
            .compactMap { $0.viewController.view as? PDFView }
            .first
    }

    private func thumbnailView(of controller: DocumentWindowController) -> PDFThumbnailView? {
        guard let contentView = controller.window?.contentView else {
            return nil
        }
        return firstSubview(of: PDFThumbnailView.self, in: contentView)
    }

    private func firstSubview<T: NSView>(of type: T.Type, in view: NSView) -> T? {
        if let view = view as? T {
            return view
        }
        return view.subviews.lazy.compactMap { firstSubview(of: type, in: $0) }.first
    }

    private func splitViewController(of controller: DocumentWindowController) -> NSSplitViewController? {
        controller.window?.contentViewController as? NSSplitViewController
    }

    private func visiblePageIndexes(of pdfView: PDFView, in document: PDFDocument) -> [Int] {
        pdfView.visiblePages.map { document.index(for: $0) }
    }

    private func waitFor<T: Equatable>(_ expected: T, until observe: () -> T) -> T {
        let deadline = Date().addingTimeInterval(1)
        var actual = observe()
        while actual != expected && Date() < deadline {
            RunLoop.current.run(until: min(deadline, Date().addingTimeInterval(0.01)))
            actual = observe()
        }
        return actual
    }
}
