import AppKit
import OSLog
import PDFKit
import QuartzCore

@MainActor
final class DocumentWindowController: NSWindowController, NSToolbarDelegate, @MainActor PDFViewDelegate {
    private static let defaultContentSize = NSSize(width: 1120, height: 780)
    private static let motionDuration: TimeInterval = 0.3
    private static let motionTimingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
    private static let widgetAnnotationType = String(PDFAnnotationSubtype.widget.rawValue.dropFirst())
    private static let zoomStep: CGFloat = 1.189207115

    private let pdfView = PDFView()
    private let sidebar = NSVisualEffectView()
    private let splitViewController = NSSplitViewController()
    private var thumbnailView: PDFThumbnailView?
    private var pageStatus: NSTextField?
    private var zoomTarget: CGFloat?
    private var firstVisibleInterval: OSSignpostIntervalState?
    private struct MatchSet {
        private(set) var selections: [PDFSelection] = []
        private(set) var activeIndex: Int?

        var isEmpty: Bool { selections.isEmpty }

        mutating func append(_ selection: PDFSelection) {
            selections.append(selection)
            activeIndex = activeIndex ?? 0
        }

        mutating func clear() {
            selections.removeAll()
            activeIndex = nil
        }

        mutating func advance(by offset: Int) {
            guard !selections.isEmpty else {
                return
            }
            let current = activeIndex ?? 0
            activeIndex = ((current + offset) % selections.count + selections.count) % selections.count
        }
    }

    @MainActor
    private final class FindSession {
        var matches = MatchSet()
        var observers: [NSObjectProtocol] = []
        var interval: OSSignpostIntervalState?
        var inFlight = true
        var refreshScheduled = false
    }

    private var findSession: FindSession?
    private var findToolbarItem: NSSearchToolbarItem?
    private var findStatus: NSTextField?

    init(document: PDFDocument) {
        let window = NSWindow(
            contentRect: NSRect(origin: .zero, size: Self.defaultContentSize),
            styleMask: [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView],
            backing: .buffered,
            defer: false
        )
        let sourceURL = document.documentURL
        window.title = sourceURL?.lastPathComponent ?? "PDF Goat"
        window.representedURL = sourceURL
        window.minSize = NSSize(width: 720, height: 500)
        window.tabbingMode = .preferred
        window.titlebarSeparatorStyle = .automatic

        super.init(window: window)

        configureDocumentView()
        window.initialFirstResponder = pdfView
        configureToolbar()
        observePageChanges()
        observeWindowClosing()
        window.setContentSize(Self.defaultContentSize)
        window.center()
        attach(document)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func showWindow(_ sender: Any?) {
        let firstShow = window.map { !$0.isVisible && !$0.isMiniaturized } ?? true
        super.showWindow(sender)
        window?.displayIfNeeded()
        if firstShow {
            LaunchTrace.signposter.emitEvent("window.shown")
        }
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
    }

    /// Drops the thumbnail view and its PDFKit-rendered images. The only
    /// memory the app can hand back under pressure; `linkThumbnailSidebar`
    /// rebuilds it.
    func releaseThumbnailSidebar() {
        thumbnailView?.removeFromSuperview()
        thumbnailView = nil
    }

    @objc func toggleSidebar(_ sender: NSObject?) {
        splitViewController.toggleSidebar(sender)
    }

    @objc func previousPage(_: NSObject?) {
        navigate(by: -1)
    }

    @objc func nextPage(_: NSObject?) {
        navigate(by: 1)
    }

    private func navigate(by offset: Int) {
        guard let document = pdfView.document, let currentPage = pdfView.currentPage else {
            return
        }
        let current = document.index(for: currentPage)
        let target = min(max(current + offset, 0), document.pageCount - 1)
        guard target != current, let page = document.page(at: target) else {
            return
        }
        pdfView.go(to: page)
    }

    @objc func zoomInPage(_: NSObject?) {
        zoom(by: Self.zoomStep)
    }

    @objc func zoomOutPage(_: NSObject?) {
        zoom(by: 1 / Self.zoomStep)
    }

    @objc func fitPage(_: NSObject?) {
        setScaleFactor(pdfView.scaleFactorForSizeToFit, restoresAutoScale: true)
    }

    @objc func revealFind(_: NSObject?) {
        findToolbarItem?.beginSearchInteraction()
    }

    @objc func findNext(_: NSObject?) {
        advanceMatch(by: 1)
    }

    @objc func findPrevious(_: NSObject?) {
        advanceMatch(by: -1)
    }

    @objc func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(revealFind(_:)) {
            return findToolbarItem != nil
        }
        if menuItem.action == #selector(findNext(_:)) || menuItem.action == #selector(findPrevious(_:)) {
            return !(findSession?.matches.isEmpty ?? true)
        }
        return true
    }

    private func zoom(by multiplier: CGFloat) {
        let current = zoomTarget ?? pdfView.scaleFactor
        let target = min(max(current * multiplier, pdfView.minScaleFactor), pdfView.maxScaleFactor)
        setScaleFactor(target)
    }

    private func setScaleFactor(_ target: CGFloat, restoresAutoScale: Bool = false) {
        pdfView.autoScales = false
        zoomTarget = target
        performSmoothly {
            pdfView.animator().scaleFactor = target
        } completion: { [weak self] in
            guard let self, zoomTarget == target else {
                return
            }
            zoomTarget = nil
            if restoresAutoScale {
                pdfView.autoScales = true
            }
        }
    }

    private func performSmoothly(_ changes: () -> Void, completion: @escaping () -> Void) {
        NSAnimationContext.runAnimationGroup { context in
            context.duration = NSWorkspace.shared.accessibilityDisplayShouldReduceMotion
                ? 0
                : Self.motionDuration
            context.timingFunction = Self.motionTimingFunction
            context.completionHandler = completion
            changes()
        }
    }

    @objc private func presentOpenPanel(_ sender: NSObject?) {
        (NSApp.delegate as? ApplicationDelegate)?.presentOpenPanel(sender)
    }

    @objc private func pageChanged(_: Notification) {
        makeVisibleAnnotationsReadOnly()
        updatePageStatus()
    }

    @objc private func visiblePagesChanged(_: Notification) {
        makeVisibleAnnotationsReadOnly()
        // PDFKit posts the first notification before `visiblePages` is
        // populated, so the first layout finishes one turn later.
        guard let interval = firstVisibleInterval else {
            return
        }
        firstVisibleInterval = nil
        DispatchQueue.main.async { [weak self] in
            LaunchTrace.signposter.endInterval("first.visible", interval)
            self?.finishFirstLayout()
        }
    }

    @objc private func liveScrollStarted(_: Notification) {
        pdfView.interpolationQuality = .low
    }

    @objc private func liveScrollEnded(_: Notification) {
        pdfView.interpolationQuality = .high
    }

    func pdfViewWillClick(onLink _: PDFView, with _: URL) {
        guard let window, window.attachedSheet == nil else {
            return
        }

        let alert = NSAlert()
        alert.messageText = "External Link Blocked"
        alert.informativeText = "PDF Goat does not let documents open apps or websites."
        alert.alertStyle = .informational
        alert.addButton(withTitle: "OK")
        alert.beginSheetModal(for: window)
    }

    private func configureDocumentView() {
        pdfView.delegate = self
        pdfView.displayMode = .singlePageContinuous
        pdfView.displayDirection = .vertical
        pdfView.displaysPageBreaks = true
        pdfView.pageShadowsEnabled = true
        pdfView.autoScales = true
        pdfView.interpolationQuality = .high
        pdfView.animations = ["scaleFactor": CABasicAnimation()]
        sidebar.material = .sidebar
        sidebar.blendingMode = .behindWindow
        sidebar.state = .followsWindowActiveState

        let sidebarController = NSViewController()
        sidebarController.view = sidebar
        let sidebarItem = NSSplitViewItem(sidebarWithViewController: sidebarController)
        sidebarItem.minimumThickness = 170
        sidebarItem.maximumThickness = 260
        sidebarItem.holdingPriority = .defaultHigh

        let canvasController = NSViewController()
        canvasController.view = pdfView
        let canvasItem = NSSplitViewItem(viewController: canvasController)
        canvasItem.minimumThickness = 480

        splitViewController.splitView.dividerStyle = .thin
        splitViewController.addSplitViewItem(sidebarItem)
        splitViewController.addSplitViewItem(canvasItem)
        window?.contentViewController = splitViewController
    }

    private func attach(_ document: PDFDocument) {
        LaunchTrace.signposter.withIntervalSignpost("attach.document") {
            pdfView.document = document
        }
        showDocumentStart(document)
        updatePageStatus()
        firstVisibleInterval = LaunchTrace.signposter.beginInterval("first.visible")
    }

    private func showDocumentStart(_ document: PDFDocument) {
        guard let first = document.page(at: 0) else {
            return
        }
        pdfView.go(to: first)
    }

    private func finishFirstLayout() {
        observeLiveScroll()
        makeVisibleAnnotationsReadOnly()
        linkThumbnailSidebar()
    }

    func linkThumbnailSidebar() {
        guard thumbnailView == nil else {
            return
        }
        let interval = LaunchTrace.signposter.beginInterval("sidebar.ready")
        let thumbnails = PDFThumbnailView()
        thumbnails.thumbnailSize = NSSize(width: 100, height: 130)
        thumbnails.maximumNumberOfColumns = 1
        thumbnails.allowsDragging = false
        thumbnails.allowsMultipleSelection = false
        thumbnails.backgroundColor = NSColor.unemphasizedSelectedContentBackgroundColor.withAlphaComponent(0.15)
        thumbnails.translatesAutoresizingMaskIntoConstraints = false
        sidebar.addSubview(thumbnails)

        NSLayoutConstraint.activate([
            thumbnails.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 8),
            thumbnails.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: -8),
            thumbnails.topAnchor.constraint(equalTo: sidebar.safeAreaLayoutGuide.topAnchor, constant: 8),
            thumbnails.bottomAnchor.constraint(equalTo: sidebar.bottomAnchor, constant: -8),
        ])

        thumbnails.pdfView = pdfView
        thumbnailView = thumbnails
        LaunchTrace.signposter.endInterval("sidebar.ready", interval)
    }

    private func configureToolbar() {
        let toolbar = NSToolbar(identifier: "PDFGoat.DocumentToolbar")
        toolbar.delegate = self
        toolbar.displayMode = .iconOnly
        toolbar.centeredItemIdentifiers = [.pageStatus]
        window?.toolbar = toolbar
        window?.toolbarStyle = .unified
    }

    private func observePageChanges() {
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(pageChanged(_:)),
            name: .PDFViewPageChanged,
            object: pdfView
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(visiblePagesChanged(_:)),
            name: .PDFViewVisiblePagesChanged,
            object: pdfView
        )
    }

    private func observeLiveScroll() {
        guard let scrollView = pdfView.documentView?.enclosingScrollView else {
            return
        }
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(liveScrollStarted(_:)),
            name: NSScrollView.willStartLiveScrollNotification,
            object: scrollView
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(liveScrollEnded(_:)),
            name: NSScrollView.didEndLiveScrollNotification,
            object: scrollView
        )
    }

    private func observeWindowClosing() {
        guard let window else {
            return
        }
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(windowWillClose(_:)),
            name: NSWindow.willCloseNotification,
            object: window
        )
    }

    @objc private func windowWillClose(_: Notification) {
        cancelActiveFind()
        NotificationCenter.default.removeObserver(self, name: .PDFViewPageChanged, object: pdfView)
        NotificationCenter.default.removeObserver(self, name: .PDFViewVisiblePagesChanged, object: pdfView)
        releaseThumbnailSidebar()
        // Observed on macOS 26.6.2 (25G83): canceling this pending perform before teardown avoids a PDFKit close lock.
        // Retest with: swift test --no-parallel --filter PDFGoatDocumentTests/closingDuringSearchCancelsCleanly
        NSObject.cancelPreviousPerformRequests(withTarget: pdfView)
        window?.contentViewController = nil
    }

    @objc private func findFieldAction(_ sender: NSSearchField) {
        startFind(for: sender.stringValue)
    }

    func startFind(for term: String) {
        let trimmed = term.trimmingCharacters(in: .whitespacesAndNewlines)
        cancelActiveFind()
        guard !trimmed.isEmpty, let document = pdfView.document else {
            return
        }

        let session = FindSession()
        findSession = session
        session.interval = FindTrace.signposter.beginInterval("find.query")

        let matchObserver = NotificationCenter.default.addObserver(
            forName: .PDFDocumentDidFindMatch,
            object: document,
            queue: .main // Main-queue delivery makes assumeIsolated and nonisolated(unsafe) sound below.
        ) { [weak self, weak session] notification in
            nonisolated(unsafe) let notification = notification
            MainActor.assumeIsolated {
                guard let session else {
                    return
                }
                self?.handleFindMatch(notification, session: session)
            }
        }
        let endObserver = NotificationCenter.default.addObserver(
            forName: .PDFDocumentDidEndFind,
            object: document,
            queue: .main
        ) { [weak self, weak session] _ in
            MainActor.assumeIsolated {
                guard let session else {
                    return
                }
                self?.handleFindEnd(session: session)
            }
        }
        session.observers = [matchObserver, endObserver]
        document.beginFindString(trimmed, withOptions: [.caseInsensitive])
        updateFindStatus()
    }

    private func cancelActiveFind() {
        if let session = findSession {
            tearDownFind(session, discardSession: true, cancelled: session.inFlight)
        }
        pdfView.document?.cancelFindString()
        pdfView.highlightedSelections = nil
        pdfView.clearSelection()
        updateFindStatus()
    }

    private func tearDownFind(_ session: FindSession, discardSession: Bool, cancelled: Bool) {
        session.observers.forEach(NotificationCenter.default.removeObserver)
        session.observers.removeAll()
        if let interval = session.interval {
            if cancelled {
                FindTrace.signposter.emitEvent("find.cancelled")
            }
            FindTrace.signposter.endInterval("find.query", interval)
        }
        session.interval = nil
        session.inFlight = false
        session.refreshScheduled = false
        if discardSession {
            session.matches.clear()
            if findSession === session {
                findSession = nil
            }
        }
    }

    private func handleFindMatch(_ notification: Notification, session: FindSession) {
        guard session === findSession,
              session.inFlight,
              let selection = notification.userInfo?[PDFDocumentFoundSelectionKey] as? PDFSelection
        else {
            return
        }
        let firstMatch = session.matches.isEmpty
        if firstMatch {
            FindTrace.signposter.emitEvent("find.first-hit")
        }
        session.matches.append(selection)
        if firstMatch {
            showActiveMatch(session)
            updateFindStatus()
        }
        scheduleFindRefresh(session)
    }

    private func scheduleFindRefresh(_ session: FindSession) {
        guard !session.refreshScheduled else {
            return
        }
        session.refreshScheduled = true
        DispatchQueue.main.async { [weak self, weak session] in
            MainActor.assumeIsolated {
                guard let self, let session, session === self.findSession, session.inFlight else {
                    return
                }
                session.refreshScheduled = false
                self.pdfView.highlightedSelections = session.matches.selections
                self.updateFindStatus()
            }
        }
    }

    private func handleFindEnd(session: FindSession) {
        guard session === findSession, session.inFlight else {
            return
        }
        pdfView.highlightedSelections = session.matches.selections
        tearDownFind(session, discardSession: false, cancelled: false)
        updateFindStatus()
    }

    private func showActiveMatch(_ session: FindSession) {
        guard let activeIndex = session.matches.activeIndex,
              session.matches.selections.indices.contains(activeIndex)
        else {
            return
        }
        let selection = session.matches.selections[activeIndex]
        pdfView.setCurrentSelection(selection, animate: true)
        pdfView.go(to: selection)
    }

    private func advanceMatch(by offset: Int) {
        guard let session = findSession, !session.matches.isEmpty else {
            return
        }
        session.matches.advance(by: offset)
        showActiveMatch(session)
        updateFindStatus()
    }

    var findStatusText: String? {
        findStatus?.stringValue
    }


    private func updateFindStatus() {
        guard let findStatus else {
            return
        }
        guard let session = findSession else {
            findStatus.stringValue = ""
            return
        }
        guard !session.matches.isEmpty else {
            findStatus.stringValue = session.inFlight ? "Searching…" : "No Results"
            return
        }
        let marker = session.inFlight ? "…" : ""
        let activeIndex = session.matches.activeIndex ?? 0
        findStatus.stringValue = "\(activeIndex + 1) of \(session.matches.selections.count)\(marker)"
    }

    private func makeVisibleAnnotationsReadOnly() {
        for page in pdfView.visiblePages {
            for annotation in page.annotations
            where annotation.type == Self.widgetAnnotationType && !annotation.isReadOnly {
                annotation.isReadOnly = true
            }
        }
    }

    private func updatePageStatus() {
        guard let pageStatus, let document = pdfView.document else {
            return
        }
        guard let page = pdfView.currentPage else {
            pageStatus.stringValue = "0 of \(document.pageCount)"
            return
        }

        pageStatus.stringValue = "\(document.index(for: page) + 1) of \(document.pageCount)"
    }

    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [.toggleNavigator, .openDocument, .previousPage, .pageStatus, .nextPage, .zoomOut, .zoomIn, .fitPage, .find, .findStatus, .flexibleSpace]
    }

    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [.toggleNavigator, .openDocument, .flexibleSpace, .previousPage, .pageStatus, .nextPage, .flexibleSpace, .zoomOut, .zoomIn, .fitPage, .flexibleSpace, .find, .findStatus]
    }

    func toolbarDidRemoveItem(_ notification: Notification) {
        guard let item = notification.userInfo?[NSToolbarUserInfoKey.itemKey] as? NSToolbarItem else {
            return
        }
        switch item.itemIdentifier {
        case .find:
            findToolbarItem = nil
        case .findStatus:
            findStatus = nil
        default:
            break
        }
    }

    func toolbar(
        _ toolbar: NSToolbar,
        itemForItemIdentifier itemIdentifier: NSToolbarItem.Identifier,
        willBeInsertedIntoToolbar flag: Bool
    ) -> NSToolbarItem? {
        switch itemIdentifier {
        case .toggleNavigator:
            actionItem(itemIdentifier, label: "Sidebar", symbol: "sidebar.left", action: #selector(toggleSidebar(_:)))
        case .openDocument:
            actionItem(itemIdentifier, label: "Open", symbol: "folder", action: #selector(presentOpenPanel(_:)))
        case .previousPage:
            actionItem(itemIdentifier, label: "Previous Page", symbol: "chevron.up", action: #selector(previousPage(_:)))
        case .pageStatus:
            statusItem(itemIdentifier, inserted: flag)
        case .nextPage:
            actionItem(itemIdentifier, label: "Next Page", symbol: "chevron.down", action: #selector(nextPage(_:)))
        case .zoomOut:
            actionItem(itemIdentifier, label: "Zoom Out", symbol: "minus.magnifyingglass", action: #selector(zoomOutPage(_:)))
        case .zoomIn:
            actionItem(itemIdentifier, label: "Zoom In", symbol: "plus.magnifyingglass", action: #selector(zoomInPage(_:)))
        case .fitPage:
            actionItem(itemIdentifier, label: "Fit Page", symbol: "arrow.up.left.and.arrow.down.right", action: #selector(fitPage(_:)))
        case .find:
            findItem(itemIdentifier, inserted: flag)
        case .findStatus:
            findStatusItem(itemIdentifier, inserted: flag)
        default:
            nil
        }
    }

    private func actionItem(
        _ identifier: NSToolbarItem.Identifier,
        label: String,
        symbol: String,
        action: Selector
    ) -> NSToolbarItem {
        let item = NSToolbarItem(itemIdentifier: identifier)
        item.label = label
        item.paletteLabel = label
        item.toolTip = label
        item.image = NSImage(systemSymbolName: symbol, accessibilityDescription: label)
        item.target = self
        item.action = action
        return item
    }

    private func statusItem(_ identifier: NSToolbarItem.Identifier, inserted: Bool) -> NSToolbarItem {
        let status = NSTextField(labelWithString: "")
        status.alignment = .center
        status.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
        status.textColor = .secondaryLabelColor
        status.setAccessibilityIdentifier("PDFGoat.pageStatus")

        let item = NSToolbarItem(itemIdentifier: identifier)
        item.label = "Page"
        item.paletteLabel = "Page"
        item.view = status

        if inserted {
            pageStatus = status
            updatePageStatus()
        }

        return item
    }

    private func findItem(_ identifier: NSToolbarItem.Identifier, inserted: Bool) -> NSToolbarItem {
        let search = NSSearchField()
        search.placeholderString = "Find in Document"
        search.target = self
        search.action = #selector(findFieldAction(_:))
        search.recentsAutosaveName = nil
        // Send the action only on Return or the search button, never on the
        // built-in incremental-search delay timer. Without this, setting the
        // field's text (by typing or by Accessibility) already submits a
        // query, and the following Return submits a second, separate one.
        search.sendsWholeSearchString = true
        search.setAccessibilityIdentifier("PDFGoat.findField")

        let item = NSSearchToolbarItem(itemIdentifier: identifier)
        item.searchField = search
        item.label = "Find"
        item.paletteLabel = "Find"
        item.toolTip = "Find in Document"

        if inserted {
            findToolbarItem = item
        }

        return item
    }

    private func findStatusItem(_ identifier: NSToolbarItem.Identifier, inserted: Bool) -> NSToolbarItem {
        let status = NSTextField(labelWithString: "")
        status.alignment = .center
        status.font = .monospacedDigitSystemFont(ofSize: 11, weight: .regular)
        status.textColor = .secondaryLabelColor
        status.setAccessibilityIdentifier("PDFGoat.findStatus")

        let item = NSToolbarItem(itemIdentifier: identifier)
        item.label = "Matches"
        item.paletteLabel = "Matches"
        item.view = status

        if inserted {
            findStatus = status
            updateFindStatus()
        }

        return item
    }
}

private extension NSToolbarItem.Identifier {
    static let toggleNavigator = Self("PDFGoat.toggleNavigator")
    static let openDocument = Self("PDFGoat.openDocument")
    static let previousPage = Self("PDFGoat.previousPage")
    static let pageStatus = Self("PDFGoat.pageStatus")
    static let nextPage = Self("PDFGoat.nextPage")
    static let zoomOut = Self("PDFGoat.zoomOut")
    static let zoomIn = Self("PDFGoat.zoomIn")
    static let fitPage = Self("PDFGoat.fitPage")
    static let find = Self("PDFGoat.find")
    static let findStatus = Self("PDFGoat.findStatus")
}
