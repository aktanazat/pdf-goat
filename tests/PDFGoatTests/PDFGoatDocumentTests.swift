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

    @Test("Page navigation moves forward, backward, and chains repeated next commands")
    func pageNavigationMovesAndChains() throws {
        let (controller, document) = shownController(pages: [PDFPage(), PDFPage(), PDFPage()])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let currentPageIndex: () -> Int = {
            guard let page = pdfView.currentPage else { return -1 }
            return document.index(for: page)
        }

        controller.nextPage(nil)
        #expect(waitFor(1, until: currentPageIndex) == 1)

        controller.previousPage(nil)
        #expect(waitFor(0, until: currentPageIndex) == 0)

        controller.nextPage(nil)
        controller.nextPage(nil)
        #expect(waitFor(2, until: currentPageIndex) == 2)
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

    @Test("Removing find toolbar items clears their retained controls")
    func removingFindToolbarItemsClearsRetainedControls() throws {
        let (controller, _) = shownController(pages: [PDFPage()])
        let toolbar = try #require(controller.window?.toolbar)
        let revealFind = NSMenuItem(
            title: "Find",
            action: #selector(DocumentWindowController.revealFind(_:)),
            keyEquivalent: "f"
        )
        let findIdentifier = NSToolbarItem.Identifier("PDFGoat.find")
        let findStatusIdentifier = NSToolbarItem.Identifier("PDFGoat.findStatus")
        let findIndex = try #require(toolbar.items.firstIndex { $0.itemIdentifier == findIdentifier })
        let findStatusIndex = try #require(toolbar.items.firstIndex { $0.itemIdentifier == findStatusIdentifier })
        defer {
            if toolbar.items.firstIndex(where: { $0.itemIdentifier == findIdentifier }) == nil {
                toolbar.insertItem(withItemIdentifier: findIdentifier, at: min(findIndex, toolbar.items.count))
            }
            if toolbar.items.firstIndex(where: { $0.itemIdentifier == findStatusIdentifier }) == nil {
                toolbar.insertItem(withItemIdentifier: findStatusIdentifier, at: min(findStatusIndex, toolbar.items.count))
            }
            controller.close()
        }

        #expect(controller.validateMenuItem(revealFind))
        toolbar.removeItem(at: findIndex)
        #expect(controller.validateMenuItem(revealFind) == false)

        let currentFindStatusIndex = try #require(toolbar.items.firstIndex { $0.itemIdentifier == findStatusIdentifier })
        toolbar.removeItem(at: currentFindStatusIndex)
        #expect(controller.findStatusText == nil)
    }
    @Test("A present search term is found through the real PDFKit notification path and highlighted")
    func presentTermIsFoundAndHighlighted() throws {
        let matchPage = try makeTextPage("Apple pie recipe")
        let otherPage = try makeTextPage("Nothing relevant here")
        let (controller, _) = shownController(pages: [matchPage, otherPage])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))

        controller.startFind(for: "Apple")
        #expect(controller.findStatusText == "Searching…")
        #expect(waitUntil { controller.findStatusText == "1 of 1" }, "observed \(controller.findStatusText ?? "<nil>")")
        #expect(waitUntil { pdfView.highlightedSelections?.count == 1 }, "observed \(pdfView.highlightedSelections?.count as Any)")

        let selections = try #require(pdfView.highlightedSelections)
        #expect(selections.first?.pages.first === matchPage)
    }

    @Test("An absent search term reports no matches once the real search completes")
    func absentTermReportsNoMatches() throws {
        let page = try makeTextPage("Nothing relevant here")
        let (controller, document) = shownController(pages: [page])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))

        let ended = waitForFindEnd(document) { controller.startFind(for: "Banana") }
        #expect(ended, "observed end notification \(ended)")
        #expect(pdfView.highlightedSelections?.isEmpty ?? true)
        #expect(controller.findStatusText == "No Results")
    }

    @Test("Rapid query replacement discards the cancelled generation's matches")
    func rapidReplacementDiscardsStaleMatches() throws {
        let applePage = try makeTextPage("Apple pie recipe")
        let bananaPage = try makeTextPage("Banana bread recipe")
        let (controller, _) = shownController(pages: [applePage, bananaPage])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))

        controller.startFind(for: "Apple")
        controller.startFind(for: "Banana")
        #expect(waitUntil { pdfView.highlightedSelections?.count == 1 }, "observed \(pdfView.highlightedSelections?.count as Any)")

        let selections = try #require(pdfView.highlightedSelections)
        #expect(selections.count == 1)
        #expect(selections.first?.pages.first === bananaPage)
    }

    @Test("Find Next then Find Previous returns to the original match")
    func findNavigationRoundTrips() throws {
        let firstPage = try makeTextPage("Apple pie recipe")
        let secondPage = try makeTextPage("A second apple tart")
        let thirdPage = try makeTextPage("A third apple pie")
        let (controller, _) = shownController(pages: [firstPage, secondPage, thirdPage])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))

        controller.startFind(for: "apple")
        #expect(waitUntil { controller.findStatusText == "1 of 3" }, "observed \(controller.findStatusText ?? "<nil>")")
        #expect(waitUntil { pdfView.highlightedSelections?.count == 3 }, "observed \(pdfView.highlightedSelections?.count as Any)")
        let firstSelection = try #require(pdfView.currentSelection)
        #expect(firstSelection.pages.first === firstPage)

        controller.findNext(nil)
        #expect(controller.findStatusText == "2 of 3")
        let secondSelection = try #require(pdfView.currentSelection)
        #expect(secondSelection.pages.first === secondPage)

        controller.findPrevious(nil)
        #expect(controller.findStatusText == "1 of 3")
        let restoredSelection = try #require(pdfView.currentSelection)
        #expect(restoredSelection.pages.first === firstPage)
        controller.findPrevious(nil)
        #expect(controller.findStatusText == "3 of 3")
        let wrappedSelection = try #require(pdfView.currentSelection)
        #expect(wrappedSelection.pages.first === thirdPage)
    }

    @Test("A running search marks provisional match counts until completion")
    func runningSearchReportsProvisionalCount() throws {
        let pages = try (0..<50).map { _ in try makeTextPage("Apple pie recipe") }
        let (controller, _) = shownController(pages: pages)
        defer { controller.close() }

        controller.startFind(for: "Apple")

        #expect(waitUntil {
            guard let status = controller.findStatusText else {
                return false
            }
            return status.contains(" of ") && status.hasSuffix("…")
        }, "observed \(controller.findStatusText ?? "<nil>")")
        #expect(waitUntil {
            guard let status = controller.findStatusText else {
                return false
            }
            return status.contains(" of ") && !status.hasSuffix("…")
        }, "observed \(controller.findStatusText ?? "<nil>")")
        #expect(controller.findStatusText?.hasSuffix("…") == false)
    }

    @Test("Clearing an active query clears highlights, selection, and status")
    func clearingQueryClearsState() throws {
        let page = try makeTextPage("Apple pie recipe")
        let (controller, _) = shownController(pages: [page])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))

        controller.startFind(for: "Apple")
        #expect(waitUntil { pdfView.highlightedSelections?.count == 1 }, "observed \(pdfView.highlightedSelections?.count as Any)")
        #expect(pdfView.currentSelection != nil)

        controller.startFind(for: "")

        #expect(waitUntil { pdfView.highlightedSelections == nil }, "observed \(pdfView.highlightedSelections as Any)")
        #expect(pdfView.currentSelection == nil)
        #expect(controller.findStatusText == "")
    }

    @Test("Closing the window during an active search cancels it without resurrecting stale matches")
    func closingDuringSearchCancelsCleanly() async throws {
        let page = try makeTextPage("Apple pie recipe")
        let (controller, document) = shownController(pages: [page])
        let pdfView = try #require(displayedPDFView(of: controller))

        controller.startFind(for: "apple")
        #expect(controller.findStatusText == "Searching…")
        controller.close()

        #expect(pdfView.highlightedSelections == nil, "observed \(pdfView.highlightedSelections as Any)")
        #expect(pdfView.currentSelection == nil, "observed \(pdfView.currentSelection as Any)")
        let lateSelection = try #require(page.selection(for: page.bounds(for: .mediaBox)))
        NotificationCenter.default.post(
            name: .PDFDocumentDidFindMatch,
            object: document,
            userInfo: [PDFDocumentFoundSelectionKey: lateSelection]
        )
        await nextMainQueueTurn()
        #expect(pdfView.highlightedSelections == nil, "observed \(pdfView.highlightedSelections as Any)")
    }

    @Test("Find navigation items disable without a query or matches")
    func findNavigationItemsRequireMatches() throws {
        let page = try makeTextPage("Apple pie recipe")
        let (controller, document) = shownController(pages: [page])
        defer { controller.close() }

        let nextItem = NSMenuItem(
            title: "Find Next",
            action: #selector(DocumentWindowController.findNext(_:)),
            keyEquivalent: ""
        )
        let previousItem = NSMenuItem(
            title: "Find Previous",
            action: #selector(DocumentWindowController.findPrevious(_:)),
            keyEquivalent: ""
        )

        #expect(controller.validateMenuItem(nextItem) == false)
        #expect(controller.validateMenuItem(previousItem) == false)

        let ended = waitForFindEnd(document) { controller.startFind(for: "Banana") }
        #expect(ended, "observed end notification \(ended)")
        #expect(controller.validateMenuItem(nextItem) == false)
        #expect(controller.validateMenuItem(previousItem) == false)
        let presentEnded = waitForFindEnd(document) { controller.startFind(for: "Apple") }
        #expect(presentEnded, "observed end notification \(presentEnded)")
        #expect(controller.validateMenuItem(previousItem) == true)
        #expect(controller.validateMenuItem(nextItem) == true)
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

    private func waitUntil(timeout: TimeInterval = 1, _ predicate: () -> Bool) -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while !predicate(), Date() < deadline {
            RunLoop.current.run(until: min(deadline, Date().addingTimeInterval(0.01)))
        }
        return predicate()
    }

    /// Waits for the real `PDFDocument.PDFDocumentDidEndFind` notification that PDFKit's own
    /// asynchronous find machinery posts, rather than polling for the absence of a result (which
    /// cannot distinguish "no matches" from "still searching").
    private func waitForFindEnd(_ document: PDFDocument, startingFind start: () -> Void) -> Bool {
        nonisolated(unsafe) var ended = false
        let observer = NotificationCenter.default.addObserver(
            forName: .PDFDocumentDidEndFind,
            object: document,
            queue: .main
        ) { _ in ended = true }
        defer { NotificationCenter.default.removeObserver(observer) }
        start()
        return waitUntil { ended }
    }

    /// Draws real text into a PDF content stream (not a rasterized image), so PDFKit's own text
    /// extraction and find machinery can genuinely locate it, exercising the real notification
    /// path rather than a mock.
    private func makeTextPage(_ text: String) throws -> PDFPage {
        let data = NSMutableData()
        var mediaBox = CGRect(x: 0, y: 0, width: 300, height: 200)
        guard let consumer = CGDataConsumer(data: data as CFMutableData),
              let context = CGContext(consumer: consumer, mediaBox: &mediaBox, nil)
        else {
            throw TestSetupError.pdfContextUnavailable
        }
        context.beginPDFPage(nil)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: false)
        NSAttributedString(string: text, attributes: [.font: NSFont.systemFont(ofSize: 18)])
            .draw(at: NSPoint(x: 10, y: 90))
        NSGraphicsContext.restoreGraphicsState()
        context.endPDFPage()
        context.closePDF()
        guard let generated = PDFDocument(data: data as Data), let page = generated.page(at: 0) else {
            throw TestSetupError.pdfContextUnavailable
        }
        return page
    }

    private enum TestSetupError: Error {
        case pdfContextUnavailable
    }
}
