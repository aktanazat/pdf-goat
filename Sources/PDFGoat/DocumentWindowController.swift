import AppKit
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
    private let thumbnailView = PDFThumbnailView()
    private let splitViewController = NSSplitViewController()
    private var pageStatus: NSTextField?
    private var pageTargetIndex: Int?
    private var zoomTarget: CGFloat?
    private var protectedPages = Set<ObjectIdentifier>()
    private var deferredSetupStarted = false

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

        configureDocumentView(document)
        window.initialFirstResponder = pdfView
        configureToolbar()
        observePageChanges()
        updatePageStatus()
        window.setContentSize(Self.defaultContentSize)
        window.center()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        nil
    }

    override func showWindow(_ sender: Any?) {
        super.showWindow(sender)
        window?.displayIfNeeded()
        guard !deferredSetupStarted else {
            return
        }
        deferredSetupStarted = true
        DispatchQueue.main.async { [weak self] in
            guard let self else {
                return
            }
            thumbnailView.pdfView = pdfView
            makeVisibleAnnotationsReadOnly()
        }
    }

    deinit {
        NotificationCenter.default.removeObserver(self)
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
        let current = pageTargetIndex ?? document.index(for: currentPage)
        let target = min(max(current + offset, 0), document.pageCount - 1)
        guard target != current, let page = document.page(at: target) else {
            return
        }

        pageTargetIndex = target
        guard
            let documentView = pdfView.documentView,
            let scrollView = documentView.enclosingScrollView
        else {
            pdfView.go(to: page)
            pageTargetIndex = nil
            return
        }

        let pageBounds = pdfView.convert(page.bounds(for: .cropBox), from: page)
        let pageFrame = documentView.convert(pageBounds, from: pdfView)
        var destination = scrollView.contentView.bounds
        destination.origin.y = pageFrame.maxY - destination.height + pdfView.pageBreakMargins.top
        let destinationOrigin = scrollView.contentView.constrainBoundsRect(destination).origin
        performSmoothly {
            scrollView.contentView.animator().setBoundsOrigin(destinationOrigin)
        } completion: { [weak self] in
            guard let self, pageTargetIndex == target else {
                return
            }
            pageTargetIndex = nil
        }
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

    private func performSmoothly(_ changes: () -> Void, completion: (() -> Void)? = nil) {
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

    private func configureDocumentView(_ document: PDFDocument) {
        pdfView.delegate = self
        pdfView.displayMode = .singlePageContinuous
        pdfView.displayDirection = .vertical
        pdfView.displaysPageBreaks = true
        pdfView.pageShadowsEnabled = true
        pdfView.autoScales = true
        pdfView.animations = ["scaleFactor": CABasicAnimation()]
        thumbnailView.thumbnailSize = NSSize(width: 100, height: 130)
        thumbnailView.maximumNumberOfColumns = 1
        thumbnailView.allowsDragging = false
        thumbnailView.allowsMultipleSelection = false
        thumbnailView.backgroundColor = NSColor.unemphasizedSelectedContentBackgroundColor.withAlphaComponent(0.15)

        let sidebarController = NSViewController()
        sidebarController.view = makeSidebar()
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
        pdfView.document = document
    }

    private func makeSidebar() -> NSView {
        let sidebar = NSVisualEffectView()
        sidebar.material = .sidebar
        sidebar.blendingMode = .behindWindow
        sidebar.state = .followsWindowActiveState
        thumbnailView.translatesAutoresizingMaskIntoConstraints = false
        sidebar.addSubview(thumbnailView)

        NSLayoutConstraint.activate([
            thumbnailView.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 8),
            thumbnailView.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: -8),
            thumbnailView.topAnchor.constraint(equalTo: sidebar.safeAreaLayoutGuide.topAnchor, constant: 8),
            thumbnailView.bottomAnchor.constraint(equalTo: sidebar.bottomAnchor, constant: -8),
        ])

        return sidebar
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

    private func makeVisibleAnnotationsReadOnly() {
        for page in pdfView.visiblePages
        where protectedPages.insert(ObjectIdentifier(page)).inserted {
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
        let status = NSTextField(labelWithString: "")
        status.alignment = .center
        status.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
        status.textColor = .secondaryLabelColor

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
