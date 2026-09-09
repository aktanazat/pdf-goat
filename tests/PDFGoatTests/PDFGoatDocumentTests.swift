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

        controller.nextPage(nil)
        #expect(waitFor(1, until: { currentPageIndex(of: pdfView, in: document) }) == 1)

        controller.previousPage(nil)
        #expect(waitFor(0, until: { currentPageIndex(of: pdfView, in: document) }) == 0)

        controller.nextPage(nil)
        controller.nextPage(nil)
        #expect(waitFor(2, until: { currentPageIndex(of: pdfView, in: document) }) == 2)
    }

    @Test("Leaving an unchanged page field preserves navigation made elsewhere")
    func leavingUnchangedPageFieldPreservesNavigation() throws {
        let (controller, document) = shownController(pages: (0..<5).map { _ in PDFPage() })
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let field = try #require(pageField(of: controller))

        controller.focusPageField(nil)
        try #require(field.currentEditor() != nil)
        controller.nextPage(nil)
        controller.nextPage(nil)
        #expect(waitFor(2, until: { currentPageIndex(of: pdfView, in: document) }) == 2)

        _ = controller.window?.makeFirstResponder(pdfView)

        #expect(waitFor(2, until: { currentPageIndex(of: pdfView, in: document) }) == 2)
        #expect(waitFor("3", until: { field.stringValue }) == "3")
    }

    @Test("A later page navigation wins over a pending page-field request")
    func laterNavigationWinsOverPendingPageRequest() throws {
        let (controller, document) = shownController(document: romanNumeralLabeledDocument(pageCount: 2))
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let field = try #require(pageField(of: controller))
        let suffix = try #require(pageSuffixField(of: controller))

        controller.focusPageField(nil)
        let editor = try #require(field.currentEditor())
        editor.selectAll(nil)
        editor.insertText("1")
        controller.nextPage(nil)
        #expect(waitFor(1, until: { currentPageIndex(of: pdfView, in: document) }) == 1)

        _ = controller.window?.makeFirstResponder(pdfView)

        #expect(waitFor(1, until: { currentPageIndex(of: pdfView, in: document) }) == 1)
        #expect(waitFor("2", until: { field.stringValue }) == "2")
        #expect(waitFor("of 2 · ii", until: { suffix.stringValue }) == "of 2 · ii")
    }

    @Test("A page change while editing refreshes both indicator halves")
    func pageChangeWhileEditingRefreshesIndicator() throws {
        let (controller, document) = shownController(document: romanNumeralLabeledDocument(pageCount: 2))
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let field = try #require(pageField(of: controller))
        let suffix = try #require(pageSuffixField(of: controller))

        controller.focusPageField(nil)
        try #require(field.currentEditor() != nil)
        controller.nextPage(nil)

        #expect(waitFor(1, until: { currentPageIndex(of: pdfView, in: document) }) == 1)
        #expect(field.currentEditor() != nil)
        #expect(waitFor("2", until: { field.stringValue }) == "2")
        #expect(waitFor("of 2 · ii", until: { suffix.stringValue }) == "of 2 · ii")
    }

    @Test("Focusing the page field after an aborted edit clears the stale request")
    func focusAfterAbortedPageEditClearsStaleRequest() throws {
        let (controller, document) = shownController(pages: (0..<5).map { _ in PDFPage() })
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let field = try #require(pageField(of: controller))
        let window = try #require(controller.window)

        _ = window.makeFirstResponder(field)
        let editor = try #require(field.currentEditor())
        editor.selectAll(nil)
        editor.insertText("5")
        field.abortEditing()
        _ = window.makeFirstResponder(pdfView)
        try #require(window.makeFirstResponder(field))
        try #require(field.currentEditor() != nil)
        _ = window.makeFirstResponder(pdfView)

        #expect(waitFor(0, until: { currentPageIndex(of: pdfView, in: document) }) == 0)
        #expect(waitFor("1", until: { field.stringValue }) == "1")
    }

    @Test("Committing the current page re-centers it at the page top")
    func samePageCommitRecentersCurrentPage() throws {
        let (controller, document) = shownController(pages: (0..<5).map { _ in PDFPage() })
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let field = try #require(pageField(of: controller))
        let scrollView = try #require(pdfView.documentView?.enclosingScrollView)

        try commit("3", intoPageFieldOf: controller, field: field)
        #expect(waitFor(2, until: { currentPageIndex(of: pdfView, in: document) }) == 2)

        let startOffset = scrollView.contentView.bounds.origin.y
        scrollView.contentView.scroll(to: NSPoint(x: 0, y: startOffset + 120))
        scrollView.reflectScrolledClipView(scrollView.contentView)
        let scrolledOffset = scrollView.contentView.bounds.origin.y
        #expect(scrolledOffset > startOffset)

        try commit("3", intoPageFieldOf: controller, field: field)

        #expect(waitFor(true, until: {
            abs(scrollView.contentView.bounds.origin.y - startOffset) < 1
        }) == true)
        #expect(currentPageIndex(of: pdfView, in: document) == 2)
    }

    @Test("Tab from the page field lands on the PDF canvas")
    func tabFromPageFieldLandsOnCanvas() throws {
        let (controller, _) = shownController(pages: [PDFPage(), PDFPage()])
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        let field = try #require(pageField(of: controller))
        let window = try #require(controller.window)

        _ = window.makeFirstResponder(field)
        try #require(field.currentEditor() != nil)
        let event = try #require(NSEvent.keyEvent(
            with: .keyDown,
            location: .zero,
            modifierFlags: [],
            timestamp: ProcessInfo.processInfo.systemUptime,
            windowNumber: window.windowNumber,
            context: nil,
            characters: "\t",
            charactersIgnoringModifiers: "\t",
            isARepeat: false,
            keyCode: 48
        ))
        window.sendEvent(event)

        #expect(waitFor(true, until: { window.firstResponder === pdfView }) == true)
    }

    @Test("Previous and Next Page menu and toolbar items disable at document boundaries")
    func pageNavigationMenuItemsValidateAtDocumentEnds() throws {
        let (controller, document) = shownController(pages: (0..<3).map { _ in PDFPage() })
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let previousItem = menuItem(action: #selector(DocumentWindowController.previousPage(_:)))
        let nextItem = menuItem(action: #selector(DocumentWindowController.nextPage(_:)))
        let focusItem = menuItem(action: #selector(DocumentWindowController.focusPageField(_:)))
        let toolbar = try #require(controller.window?.toolbar)
        let previousToolbarItem = try #require(toolbar.items.first { $0.label == "Previous Page" })
        let nextToolbarItem = try #require(toolbar.items.first { $0.label == "Next Page" })

        #expect(controller.validateMenuItem(focusItem) == true)
        #expect(controller.validateMenuItem(previousItem) == false)
        #expect(controller.validateMenuItem(nextItem) == true)
        #expect(controller.validateToolbarItem(previousToolbarItem) == false)
        #expect(controller.validateToolbarItem(nextToolbarItem) == true)
        toolbar.validateVisibleItems()
        #expect(previousToolbarItem.isEnabled == false)
        #expect(nextToolbarItem.isEnabled == true)

        controller.nextPage(nil)
        #expect(waitFor(1, until: { currentPageIndex(of: pdfView, in: document) }) == 1)
        #expect(controller.validateMenuItem(previousItem) == true)
        #expect(controller.validateMenuItem(nextItem) == true)
        #expect(controller.validateToolbarItem(previousToolbarItem) == true)
        #expect(controller.validateToolbarItem(nextToolbarItem) == true)

        controller.nextPage(nil)
        #expect(waitFor(2, until: { currentPageIndex(of: pdfView, in: document) }) == 2)
        #expect(controller.validateMenuItem(previousItem) == true)
        #expect(controller.validateMenuItem(nextItem) == false)
        #expect(controller.validateToolbarItem(previousToolbarItem) == true)
        #expect(controller.validateToolbarItem(nextToolbarItem) == false)
        toolbar.validateVisibleItems()
        #expect(previousToolbarItem.isEnabled == true)
        #expect(nextToolbarItem.isEnabled == false)
    }

    @Test("Zoomed-out scrolling names the topmost fully visible page and enables Previous")
    func zoomedOutScrollingNamesTopmostFullPage() throws {
        let (controller, document) = shownController(pages: (0..<12).map { _ in PDFPage() })
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        let field = try #require(pageField(of: controller))
        let suffix = try #require(pageSuffixField(of: controller))
        let window = try #require(controller.window)
        let previousItem = menuItem(action: #selector(DocumentWindowController.previousPage(_:)))
        let toolbar = try #require(window.toolbar)
        let previousToolbarItem = try #require(toolbar.items.first { $0.label == "Previous Page" })

        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        for _ in 0..<12 {
            controller.zoomOutPage(nil)
        }
        try #require(waitFor(true, until: {
            pdfView.scaleFactor < 0.3 && pdfView.visiblePages.count >= 5
        }) == true)
        #expect(waitFor("1", until: { field.stringValue }) == "1")
        toolbar.validateVisibleItems()
        #expect(previousToolbarItem.isEnabled == false)

        try scroll(pdfView, toTopOfPageAt: 2, in: document)

        #expect(waitFor("3", until: { field.stringValue }) == "3")
        #expect(waitFor("of 12", until: { suffix.stringValue }) == "of 12")
        #expect(controller.validateMenuItem(previousItem) == true)
        toolbar.validateVisibleItems()
        #expect(waitFor(true, until: { previousToolbarItem.isEnabled }) == true)
        // Several equal-height pages are fully visible at this scale, so that
        // number came out of the tie and not out of one tallest page.
        #expect(pdfView.visiblePages.count >= 5)
    }

    @Test("A scroll PDFKit does not report still refreshes the page status")
    func clipViewBoundsChangeRefreshesPageStatus() throws {
        let (controller, document) = shownController(pages: (0..<12).map { _ in PDFPage() })
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        let field = try #require(pageField(of: controller))

        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        for _ in 0..<12 {
            controller.zoomOutPage(nil)
        }
        try #require(waitFor(true, until: {
            pdfView.scaleFactor < 0.3 && pdfView.visiblePages.count >= 5
        }) == true)
        try scroll(pdfView, toTopOfPageAt: 2, in: document)
        try #require(waitFor("3", until: { field.stringValue }) == "3")
        let visiblePages = Set(visiblePageIndexes(of: pdfView, in: document))

        // Clipping the top page by three points changes which page is current
        // without changing which pages are visible, so the clip-view bounds
        // notification is the only thing left that can refresh the status.
        try scroll(pdfView, toTopOfPageAt: 2, clippedBy: 3, in: document)

        #expect(waitFor("4", until: { field.stringValue }) == "4")
        #expect(Set(visiblePageIndexes(of: pdfView, in: document)) == visiblePages)
    }

    @Test("Go to Page keeps the zoomed-out last page current and disables Next")
    func zoomedOutLastPageRemainsCurrent() throws {
        let (controller, document) = shownController(pages: fractionalHeightPages(count: 12))
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        let field = try #require(pageField(of: controller))
        let suffix = try #require(pageSuffixField(of: controller))
        let nextItem = menuItem(action: #selector(DocumentWindowController.nextPage(_:)))
        let toolbar = try #require(controller.window?.toolbar)
        let nextToolbarItem = try #require(toolbar.items.first { $0.label == "Next Page" })

        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        for _ in 0..<12 {
            controller.zoomOutPage(nil)
        }
        #expect(waitFor(true, until: {
            pdfView.scaleFactor < 0.3 && pdfView.visiblePages.count >= 5
        }) == true)

        try commit("12", intoPageFieldOf: controller, field: field)

        #expect(waitFor("12", until: { field.stringValue }) == "12")
        #expect(waitFor("of 12", until: { suffix.stringValue }) == "of 12")
        #expect(waitFor(false, until: { controller.validateMenuItem(nextItem) }) == false)
        #expect(waitFor(false, until: { nextToolbarItem.isEnabled }) == false)
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

    @Test(
        "Go to Page accepts a trimmed 1-based number and rejects zero, negative, non-numeric, and out-of-range input, reverting the field either way",
        arguments: [
            ("1", 0, "1"),
            ("3", 2, "3"),
            ("  4  ", 3, "4"),
            ("0", 0, "1"),
            ("-9223372036854775808", 0, "1"),
            ("abc", 0, "1"),
            ("99", 0, "1"),
        ]
    )
    func goToPageParsesAndBoundsInput(input: String, expectedIndex: Int, expectedFieldValue: String) throws {
        let (controller, document) = shownController(pages: (0..<5).map { _ in PDFPage() })
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let field = try #require(pageField(of: controller))

        try commit(input, intoPageFieldOf: controller, field: field)

        #expect(waitFor(expectedIndex, until: { currentPageIndex(of: pdfView, in: document) }) == expectedIndex)
        #expect(waitFor(expectedFieldValue, until: { field.stringValue }) == expectedFieldValue)
    }

    @Test("Page status shows the PDF page label only when it differs from the numeric page")
    func pageStatusShowsDivergingPageLabel() throws {
        let (controller, document) = shownController(document: romanNumeralLabeledDocument())
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let suffix = try #require(pageSuffixField(of: controller))

        #expect(waitFor("of 1 · i", until: { suffix.stringValue }) == "of 1 · i")
    }

    @Test("Page status omits a page label equal to its numeric page")
    func pageStatusOmitsMatchingPageLabel() throws {
        let (controller, document) = shownController(document: arabicNumeralLabeledDocument(pageCount: 2))
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let suffix = try #require(pageSuffixField(of: controller))

        #expect(waitFor("of 2", until: { suffix.stringValue }) == "of 2")
    }

    @Test("Back and forward keep page status and relative navigation on restored page")
    func backAndForwardTrackHistory() throws {
        let (controller, document) = shownController(pages: (0..<5).map { _ in PDFPage() })
        defer { controller.close() }
        let pdfView = try #require(displayedPDFView(of: controller))
        try #require(waitFor([0]) { visiblePageIndexes(of: pdfView, in: document) } == [0])
        let field = try #require(pageField(of: controller))
        let backItem = menuItem(action: #selector(DocumentWindowController.historyGoBack(_:)))
        let forwardItem = menuItem(action: #selector(DocumentWindowController.historyGoForward(_:)))
        let nextItem = menuItem(action: #selector(DocumentWindowController.nextPage(_:)))

        #expect(controller.validateMenuItem(backItem) == false)
        #expect(controller.validateMenuItem(forwardItem) == false)

        try commit("5", intoPageFieldOf: controller, field: field)
        #expect(waitFor(4, until: { currentPageIndex(of: pdfView, in: document) }) == 4)
        #expect(waitFor(true, until: { pdfView.canGoBack }) == true)
        #expect(controller.validateMenuItem(backItem) == true)
        #expect(controller.validateMenuItem(forwardItem) == false)

        controller.historyGoBack(nil)
        #expect(waitFor(0, until: { currentPageIndex(of: pdfView, in: document) }) == 0)
        #expect(waitFor(true, until: { pdfView.canGoForward }) == true)
        #expect(controller.validateMenuItem(forwardItem) == true)

        controller.historyGoForward(nil)
        #expect(waitFor(false, until: { pdfView.canGoForward }) == false)
        #expect(waitFor("5", until: { field.stringValue }) == "5")
        #expect(waitFor(false, until: { controller.validateMenuItem(nextItem) }) == false)

        controller.previousPage(nil)
        #expect(waitFor("4", until: { field.stringValue }) == "4")
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
        return shownController(document: document)
    }

    private func shownController(document: PDFDocument) -> (DocumentWindowController, PDFDocument) {
        let controller = DocumentWindowController(document: document)
        controller.showWindow(nil)
        controller.window?.displayIfNeeded()
        return (controller, document)
    }

    private func fractionalHeightPages(count: Int) -> [PDFPage] {
        (0..<count).map { index in
            let height: CGFloat = index == count - 1 ? 791.75 : 792
            let bounds = NSRect(x: 0, y: 0, width: 612, height: height)
            let page = PDFPage()
            page.setBounds(bounds, for: .mediaBox)
            page.setBounds(bounds, for: .cropBox)
            return page
        }
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

    /// A real PageLabels tree gives each page a lowercase-roman label.
    private func romanNumeralLabeledDocument(pageCount: Int = 1) -> PDFDocument {
        labeledDocument(pageCount: pageCount, labelStyle: "/S /r /St 1")
    }

    /// A real PageLabels tree gives each page an Arabic-numeral label.
    private func arabicNumeralLabeledDocument(pageCount: Int = 1) -> PDFDocument {
        labeledDocument(pageCount: pageCount, labelStyle: "/S /D /St 1")
    }

    private func labeledDocument(pageCount: Int, labelStyle: String) -> PDFDocument {
        var body = Data()
        var offsets: [Int] = [0]

        func appendObject(_ text: String) {
            offsets.append(body.count)
            body.append(Data(text.utf8))
        }

        body.append(Data("%PDF-1.4\n".utf8))
        appendObject("1 0 obj\n<< /Type /Catalog /Pages 2 0 R /PageLabels << /Nums [0 << \(labelStyle) >>] >> >>\nendobj\n")
        let pageReferences = (0..<pageCount).map { "\($0 + 3) 0 R" }.joined(separator: " ")
        appendObject("2 0 obj\n<< /Type /Pages /Kids [\(pageReferences)] /Count \(pageCount) >>\nendobj\n")
        for index in 0..<pageCount {
            appendObject("\(index + 3) 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Resources << >> >>\nendobj\n")
        }

        let xrefOffset = body.count
        let objectCount = pageCount + 2
        var xref = "xref\n0 \(objectCount + 1)\n0000000000 65535 f \n"
        for index in 1...objectCount {
            xref += String(format: "%010d 00000 n \n", offsets[index])
        }
        body.append(Data(xref.utf8))
        body.append(Data("trailer\n<< /Size \(objectCount + 1) /Root 1 0 R >>\nstartxref\n\(xrefOffset)\n%%EOF".utf8))

        guard let document = PDFDocument(data: body) else {
            preconditionFailure("Fixture PDF with a /PageLabels dictionary failed to parse")
        }
        return document
    }

    private func displayedPDFView(of controller: DocumentWindowController) -> PDFView? {
        splitViewController(of: controller)?
            .splitViewItems
            .compactMap { $0.viewController.view as? PDFView }
            .first
    }

    private func pageField(of controller: DocumentWindowController) -> NSTextField? {
        toolbarTextFields(of: controller).first { $0.isEditable }
    }

    private func menuItem(action: Selector) -> NSMenuItem {
        NSMenuItem(title: "", action: action, keyEquivalent: "")
    }

    private func pageSuffixField(of controller: DocumentWindowController) -> NSTextField? {
        toolbarTextFields(of: controller).first { !$0.isEditable }
    }

    /// Scrolls so the top edge of page `index` sits at the top of the visible
    /// band, less `clipped` points of that page.
    private func scroll(_ pdfView: PDFView, toTopOfPageAt index: Int, clippedBy clipped: CGFloat = 0, in document: PDFDocument) throws {
        let page = try #require(document.page(at: index))
        let scrollView = try #require(pdfView.documentView?.enclosingScrollView)
        let clipView = scrollView.contentView
        let pageRect = pdfView.convert(pdfView.convert(page.bounds(for: pdfView.displayBox), from: page), to: clipView)
        let bandTop = clipView.isFlipped ? pageRect.minY + clipped : pageRect.maxY - clipped
        let origin = clipView.isFlipped ? bandTop : bandTop - clipView.bounds.height
        clipView.scroll(to: NSPoint(x: clipView.bounds.minX, y: origin))
        scrollView.reflectScrolledClipView(clipView)
    }

    private func commit(_ text: String, intoPageFieldOf controller: DocumentWindowController, field: NSTextField) throws {
        _ = controller.window?.makeFirstResponder(field)
        let editor = try #require(field.currentEditor())
        editor.selectAll(nil)
        editor.insertText(text)
        editor.insertNewline(nil)
    }

    private func toolbarTextFields(of controller: DocumentWindowController) -> [NSTextField] {
        (controller.window?.toolbar?.items.first { $0.label == "Page" }?.view as? NSStackView)?
            .arrangedSubviews.compactMap { $0 as? NSTextField } ?? []
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

    private func currentPageIndex(of pdfView: PDFView, in document: PDFDocument) -> Int {
        pdfView.currentPage.map(document.index(for:)) ?? -1
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
