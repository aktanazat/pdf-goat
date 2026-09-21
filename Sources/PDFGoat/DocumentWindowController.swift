import AppKit
import OSLog
import PDFKit
import QuartzCore

@MainActor
private final class PageField: NSTextField {
    var onBecomeFirstResponder: (() -> Void)?

    override func becomeFirstResponder() -> Bool {
        guard super.becomeFirstResponder() else {
            return false
        }
        onBecomeFirstResponder?()
        return true
    }
}

@MainActor
final class DocumentWindowController: NSWindowController, NSToolbarDelegate, @MainActor PDFViewDelegate, NSTextFieldDelegate, NSMenuItemValidation, NSToolbarItemValidation {
    private static let defaultContentSize = NSSize(width: 1120, height: 780)
    private static let motionDuration: TimeInterval = 0.3
    private static let motionTimingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
    private static let widgetAnnotationType = String(PDFAnnotationSubtype.widget.rawValue.dropFirst())
    private static let zoomStep: CGFloat = 1.189207115

    private let pdfView = PDFView()
    private lazy var documentSearch = DocumentSearch(pdfView: pdfView)
    private let sidebar = NSVisualEffectView()
    private let splitViewController = NSSplitViewController()
    private var thumbnailView: PDFThumbnailView?
    private var pageField: NSTextField?
    private var pageSuffixField: NSTextField?
    private var pageFieldRequest: String?
    private var preferredNavigationPageIndex: Int?
    private var zoomTarget: CGFloat?
    private var firstVisibleInterval: OSSignpostIntervalState?
    private weak var observedClipView: NSClipView?

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
        pageField?.nextKeyView = pdfView
        if firstShow {
            LaunchTrace.signposter.emitEvent("window.shown")
        }
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
    }

    @objc func findWords(_: NSObject?) {
        documentSearch.show(meaning: false)
    }

    @objc func findMeaning(_: NSObject?) {
        documentSearch.show(meaning: true)
    }

    @objc func nextFindResult(_: NSObject?) {
        documentSearch.move(by: 1)
    }

    @objc func previousFindResult(_: NSObject?) {
        documentSearch.move(by: -1)
    }

    @objc private func documentWindowWillClose(_: Notification) {
        documentSearch.close()
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
        guard let page = page(offsetBy: offset) else {
            return
        }
        navigate(to: page)
    }

    private func navigate(to page: PDFPage) {
        guard let document = pdfView.document else {
            return
        }
        preferredNavigationPageIndex = document.index(for: page)
        pdfView.go(to: page)
    }

    private func page(at index: Int) -> PDFPage? {
        guard let document = pdfView.document, index >= 0, index < document.pageCount else {
            return nil
        }
        return document.page(at: index)
    }

    /// The page with the most visible height is the current page. Candidates
    /// are measured against the live scroll geometry. Heights within one
    /// backing pixel of the maximum are a visual tie: an explicit navigation
    /// destination wins while it stays inside the tie, otherwise the lowest
    /// document index wins, so one scroll position always resolves to one
    /// page. `visiblePages` is a debounced snapshot that can still name the
    /// pre-navigation page, or omit a page that just became visible after a
    /// scale change, so it only seeds the candidate set: `currentPage`'s
    /// neighbours join it, as many as fit the viewport at this scale. Before
    /// PDFKit lays out a fresh document or scroll position every height is
    /// zero, and `currentPage` is the only answer that exists for that
    /// instant.
    private func logicalPage(in document: PDFDocument) -> PDFPage? {
        var candidates = Set(pdfView.visiblePages.map { document.index(for: $0) })
        if let anchorPage = pdfView.currentPage {
            let anchor = document.index(for: anchorPage)
            let anchorHeight = pdfView.convert(anchorPage.bounds(for: pdfView.displayBox), from: anchorPage).height
            let radius = anchorHeight > 0 ? max(1, Int((pdfView.bounds.height / anchorHeight).rounded(.up)) + 1) : 1
            candidates.formUnion((anchor - radius)...(anchor + radius))
        }

        let candidatePages = candidates.sorted().compactMap { page(at: $0) }
        let maximumHeight = candidatePages.reduce(0) { max($0, visibleHeight(of: $1)) }
        guard maximumHeight > 0 else {
            return pdfView.currentPage
        }

        let tieTolerance = 1 / max(window?.backingScaleFactor ?? 1, 1)
        if let preferredIndex = preferredNavigationPageIndex {
            if candidates.contains(preferredIndex), let preferredPage = page(at: preferredIndex) {
                let preferredHeight = visibleHeight(of: preferredPage)
                if preferredHeight > 0, maximumHeight - preferredHeight <= tieTolerance {
                    return preferredPage
                }
            }
            preferredNavigationPageIndex = nil
        }
        return candidatePages.first { maximumHeight - visibleHeight(of: $0) <= tieTolerance }
    }

    /// How much of `page` the viewport shows right now, in view points.
    private func visibleHeight(of page: PDFPage) -> CGFloat {
        let pageBounds = pdfView.convert(page.bounds(for: pdfView.displayBox), from: page)
        return pdfView.bounds.intersection(pageBounds).height
    }

    private func page(offsetBy offset: Int) -> PDFPage? {
        guard let document = pdfView.document, let logical = logicalPage(in: document) else {
            return nil
        }
        return page(at: document.index(for: logical) + offset)
    }

    private func canNavigate(by offset: Int) -> Bool {
        page(offsetBy: offset) != nil
    }

    // `PDFView` implements `goBack:`, `goForward:`, and `goToPage:`, so target-less menu forwards use distinct names.
    @objc func historyGoBack(_ sender: NSObject?) {
        pdfView.goBack(sender)
    }

    @objc func historyGoForward(_ sender: NSObject?) {
        pdfView.goForward(sender)
    }

    @objc func focusPageField(_ sender: NSObject?) {
        guard let window, let pageField else {
            return
        }
        window.makeFirstResponder(pageField)
        pageField.currentEditor()?.selectAll(sender)
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
        pageFieldRequest = nil
        refreshPageStatus(updatesEditor: true)
    }

    @objc private func visiblePagesChanged(_: Notification) {
        makeVisibleAnnotationsReadOnly()
        refreshPageStatus()
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

    @objc private func scrollBoundsChanged(_: Notification) {
        refreshPageStatus()
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
        let canvas = NSView()
        let searchView = documentSearch.view
        canvas.addSubview(searchView)
        canvas.addSubview(pdfView)
        pdfView.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            searchView.topAnchor.constraint(equalTo: canvas.safeAreaLayoutGuide.topAnchor),
            searchView.leadingAnchor.constraint(equalTo: canvas.leadingAnchor),
            searchView.trailingAnchor.constraint(equalTo: canvas.trailingAnchor),
            pdfView.topAnchor.constraint(equalTo: searchView.bottomAnchor),
            pdfView.leadingAnchor.constraint(equalTo: canvas.leadingAnchor),
            pdfView.trailingAnchor.constraint(equalTo: canvas.trailingAnchor),
            pdfView.bottomAnchor.constraint(equalTo: canvas.bottomAnchor),
        ])
        canvasController.view = canvas
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
        observeLiveScroll()
        showDocumentStart(document)
        refreshPageStatus()
        firstVisibleInterval = LaunchTrace.signposter.beginInterval("first.visible")
    }

    private func showDocumentStart(_ document: PDFDocument) {
        guard let first = document.page(at: 0) else {
            return
        }
        navigate(to: first)
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
            self, selector: #selector(documentWindowWillClose(_:)),
            name: NSWindow.willCloseNotification, object: window
        )
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

    /// PDFKit builds the scroll view while the document is attached and keeps
    /// it, so the wiring runs there and re-asserts itself by clip-view
    /// identity. Waiting for the first-layout notification instead leaves the
    /// first scroll of a session unobserved.
    private func observeLiveScroll() {
        guard let scrollView = pdfView.documentView?.enclosingScrollView,
              scrollView.contentView !== observedClipView else {
            return
        }
        let clipView = scrollView.contentView
        observedClipView = clipView
        clipView.postsBoundsChangedNotifications = true
        // A replaced clip view leaves the earlier registrations in place, and
        // the scroll view keeps its own, so drop all three before re-adding.
        NotificationCenter.default.removeObserver(
            self,
            name: NSView.boundsDidChangeNotification,
            object: nil
        )
        NotificationCenter.default.removeObserver(
            self,
            name: NSScrollView.willStartLiveScrollNotification,
            object: nil
        )
        NotificationCenter.default.removeObserver(
            self,
            name: NSScrollView.didEndLiveScrollNotification,
            object: nil
        )
        NotificationCenter.default.addObserver(
            self,
            selector: #selector(scrollBoundsChanged(_:)),
            name: NSView.boundsDidChangeNotification,
            object: clipView
        )
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

    private func makeVisibleAnnotationsReadOnly() {
        for page in pdfView.visiblePages {
            for annotation in page.annotations
            where annotation.type == Self.widgetAnnotationType && !annotation.isReadOnly {
                annotation.isReadOnly = true
            }
        }
    }

    private func pageStatusStrings(for document: PDFDocument) -> (number: String, suffix: String) {
        guard let page = logicalPage(in: document) else {
            return ("", "of \(document.pageCount)")
        }
        let number = document.index(for: page) + 1
        let label = page.label?.trimmingCharacters(in: .whitespacesAndNewlines)
        let suffix = if let label, !label.isEmpty, label != String(number) {
            "of \(document.pageCount) · \(label)"
        } else {
            "of \(document.pageCount)"
        }
        return (String(number), suffix)
    }

    private func writePageStatus(updatesEditor: Bool) {
        guard let document = pdfView.document, let pageField, let pageSuffixField else {
            return
        }
        let (number, suffix) = pageStatusStrings(for: document)
        pageField.stringValue = number
        if updatesEditor {
            pageField.currentEditor()?.string = number
        }
        pageSuffixField.stringValue = suffix
    }

    private func refreshPageStatus(updatesEditor: Bool = false) {
        writePageStatus(updatesEditor: updatesEditor)
        window?.toolbar?.validateVisibleItems()
    }

    private func pageFieldDidBecomeFirstResponder() {
        pageFieldRequest = nil
        refreshPageStatus()
    }

    func controlTextDidEndEditing(_ obj: Notification) {
        guard (obj.object as? NSTextField) === pageField else {
            return
        }
        let movement = obj.userInfo?[NSText.movementUserInfoKey] as? Int
        if movement == NSTextMovement.cancel.rawValue {
            pageFieldRequest = nil
            refreshPageStatus(updatesEditor: true)
            return
        }
        commitPageField(moveFocusToPDFView: movement == NSTextMovement.return.rawValue)
    }

    func controlTextDidChange(_ obj: Notification) {
        guard let pageField, (obj.object as? NSTextField) === pageField else {
            return
        }
        pageFieldRequest = pageField.currentEditor()?.string
    }

    private func commitPageField(moveFocusToPDFView: Bool) {
        if moveFocusToPDFView {
            window?.makeFirstResponder(pdfView)
        }
        guard pdfView.document != nil else {
            pageFieldRequest = nil
            return
        }

        let request = pageFieldRequest
        pageFieldRequest = nil
        if
            let request,
            let requested = Int(request.trimmingCharacters(in: .whitespacesAndNewlines)),
            // `Int.min` parses, and `Int.min - 1` traps, so reject non-positive input
            // before converting to a 0-based index.
            requested > 0,
            let target = page(at: requested - 1)
        {
            navigate(to: target)
        }

        refreshPageStatus()
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        canPerform(menuItem.action)
    }

    func validateToolbarItem(_ toolbarItem: NSToolbarItem) -> Bool {
        canPerform(toolbarItem.action)
    }

    private func canPerform(_ action: Selector?) -> Bool {
        switch action {
        case #selector(previousPage(_:)):
            return canNavigate(by: -1)
        case #selector(nextPage(_:)):
            return canNavigate(by: 1)
        case #selector(historyGoBack(_:)):
            return pdfView.canGoBack
        case #selector(historyGoForward(_:)):
            return pdfView.canGoForward
        case #selector(focusPageField(_:)):
            return pageField?.window != nil
        case #selector(nextFindResult(_:)), #selector(previousFindResult(_:)):
            return documentSearch.hasResults
        default:
            return true
        }
    }


    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [.toggleNavigator, .openDocument, .previousPage, .pageStatus, .nextPage, .zoomOut, .zoomIn, .fitPage, .flexibleSpace]
    }

    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [.toggleNavigator, .openDocument, .flexibleSpace, .previousPage, .pageStatus, .nextPage, .flexibleSpace, .zoomOut, .zoomIn, .fitPage]
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
        let field = PageField()
        field.alignment = .right
        field.bezelStyle = .roundedBezel
        field.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
        field.placeholderString = "Page"
        field.toolTip = "Go to Page"
        field.setAccessibilityLabel("Page number")
        field.translatesAutoresizingMaskIntoConstraints = false
        field.widthAnchor.constraint(equalToConstant: 48).isActive = true

        let suffix = NSTextField(labelWithString: "")
        suffix.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
        suffix.textColor = .secondaryLabelColor
        suffix.translatesAutoresizingMaskIntoConstraints = false

        let stack = NSStackView(views: [field, suffix])
        stack.orientation = .horizontal
        stack.alignment = .centerY
        stack.spacing = 4

        let item = NSToolbarItem(itemIdentifier: identifier)
        item.label = "Page"
        item.paletteLabel = "Page"
        item.view = stack

        if inserted {
            pageField = field
            pageSuffixField = suffix
            refreshPageStatus()
            field.delegate = self
            field.onBecomeFirstResponder = { [weak self] in
                self?.pageFieldDidBecomeFirstResponder()
            }
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
}
