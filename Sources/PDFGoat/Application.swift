import AppKit
import UniformTypeIdentifiers

@main
@MainActor
enum PDFGoatApplication {
    private static let delegate = ApplicationDelegate()

    static func main() {
        let application = NSApplication.shared
        application.delegate = delegate
        application.setActivationPolicy(.regular)
        application.run()
    }
}

@MainActor
final class ApplicationDelegate: NSObject, NSApplicationDelegate {
    private var memoryPressureSource: DispatchSourceMemoryPressure?

    func applicationDidFinishLaunching(_ notification: Notification) {
        configureMainMenu()
        observeMemoryPressure()
        NSApp.activate()

        let paths = CommandLine.arguments.dropFirst().filter { !$0.hasPrefix("-") }
        if paths.isEmpty {
            DispatchQueue.main.async { [weak self] in
                guard NSDocumentController.shared.documents.isEmpty else {
                    return
                }
                self?.presentOpenPanel(nil)
            }
            return
        }

        open(paths.map { URL(fileURLWithPath: $0) })
        if NSDocumentController.shared.documents.isEmpty {
            presentOpenPanel(nil)
        }
    }

    func application(_ sender: NSApplication, openFiles filenames: [String]) {
        let succeeded = open(filenames.map { URL(fileURLWithPath: $0) })
        sender.reply(toOpenOrPrint: succeeded ? .success : .failure)
    }

    @objc func presentOpenPanel(_ sender: NSObject?) {
        let panel = NSOpenPanel()
        panel.allowedContentTypes = [.pdf]
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.prompt = "Open"

        panel.begin { [weak self] response in
            guard response == .OK else {
                return
            }
            self?.open(panel.urls)
        }
    }

    @discardableResult
    private func open(_ urls: [URL]) -> Bool {
        guard let first = urls.first else {
            return true
        }
        let succeeded = openAndPresent(first)
        if urls.count > 1 {
            openNext(urls, at: 1)
        }
        return succeeded
    }

    private func openNext(_ urls: [URL], at index: Int) {
        DispatchQueue.main.async { [weak self] in
            guard let self else {
                return
            }
            _ = openAndPresent(urls[index])
            if index + 1 < urls.count {
                openNext(urls, at: index + 1)
            }
        }
    }

    private func openAndPresent(_ url: URL) -> Bool {
        do {
            try open(url)
            return true
        } catch {
            present(error)
            return false
        }
    }

    private func open(_ url: URL) throws {
        let normalizedURL = url.standardizedFileURL.resolvingSymlinksInPath()
        let controller = NSDocumentController.shared

        if let openDocument = controller.document(for: normalizedURL) {
            openDocument.showWindows()
            return
        }

        let document = try PDFGoatDocument(sourceURL: normalizedURL)
        controller.addDocument(document)
        document.makeWindowControllers()
        document.showWindows()
    }

    private func present(_ error: Error) {
        let alert = NSAlert(error: error)
        if let window = NSApp.keyWindow {
            alert.beginSheetModal(for: window)
        } else {
            alert.runModal()
        }
    }

    private func observeMemoryPressure() {
        let source = DispatchSource.makeMemoryPressureSource(eventMask: [.normal, .warning, .critical], queue: .main)
        source.setEventHandler { [weak source] in
            MainActor.assumeIsolated {
                guard let source else {
                    return
                }
                let relieved = source.data.contains(.normal)
                for controller in NSDocumentController.shared.documents.flatMap(\.windowControllers) {
                    guard let controller = controller as? DocumentWindowController else {
                        continue
                    }
                    if relieved {
                        controller.linkThumbnailSidebar()
                    } else {
                        controller.releaseThumbnailSidebar()
                    }
                }
            }
        }
        source.activate()
        memoryPressureSource = source
    }

    private func configureMainMenu() {
        let mainMenu = NSMenu()
        mainMenu.addItem(applicationMenu())
        mainMenu.addItem(fileMenu())
        mainMenu.addItem(editMenu())
        mainMenu.addItem(viewMenu())

        let (windowItem, windowMenu) = submenu(named: "Window")
        windowMenu.addItem(withTitle: "Minimize", action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windowMenu.addItem(withTitle: "Zoom", action: #selector(NSWindow.performZoom(_:)), keyEquivalent: "")
        windowMenu.addItem(.separator())
        windowMenu.addItem(withTitle: "Bring All to Front", action: #selector(NSApplication.arrangeInFront(_:)), keyEquivalent: "")
        mainMenu.addItem(windowItem)
        NSApp.windowsMenu = windowMenu
        NSApp.mainMenu = mainMenu
    }

    private func applicationMenu() -> NSMenuItem {
        let (item, menu) = submenu(named: "PDF Goat")
        menu.addItem(withTitle: "About PDF Goat", action: #selector(NSApplication.orderFrontStandardAboutPanel(_:)), keyEquivalent: "")
        menu.addItem(.separator())
        menu.addItem(withTitle: "Hide PDF Goat", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        menu.addItem(withTitle: "Hide Others", action: #selector(NSApplication.hideOtherApplications(_:)), keyEquivalent: "h").keyEquivalentModifierMask = [.command, .option]
        menu.addItem(withTitle: "Show All", action: #selector(NSApplication.unhideAllApplications(_:)), keyEquivalent: "")
        menu.addItem(.separator())
        menu.addItem(withTitle: "Quit PDF Goat", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        return item
    }

    private func fileMenu() -> NSMenuItem {
        let (item, menu) = submenu(named: "File")
        let openItem = menu.addItem(withTitle: "Open…", action: #selector(presentOpenPanel(_:)), keyEquivalent: "o")
        openItem.target = self
        menu.addItem(.separator())
        menu.addItem(withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        return item
    }

    private func editMenu() -> NSMenuItem {
        let (item, menu) = submenu(named: "Edit")
        menu.addItem(withTitle: "Copy", action: #selector(NSText.copy(_:)), keyEquivalent: "c")
        menu.addItem(withTitle: "Select All", action: #selector(NSText.selectAll(_:)), keyEquivalent: "a")
        return item
    }

    private func viewMenu() -> NSMenuItem {
        let (item, menu) = submenu(named: "View")
        menu.addItem(withTitle: "Toggle Sidebar", action: #selector(DocumentWindowController.toggleSidebar(_:)), keyEquivalent: "s").keyEquivalentModifierMask = [.command, .control]
        menu.addItem(.separator())
        menu.addItem(withTitle: "Zoom In", action: #selector(DocumentWindowController.zoomInPage(_:)), keyEquivalent: "+")
        menu.addItem(withTitle: "Zoom Out", action: #selector(DocumentWindowController.zoomOutPage(_:)), keyEquivalent: "-")
        menu.addItem(withTitle: "Fit Page", action: #selector(DocumentWindowController.fitPage(_:)), keyEquivalent: "0")
        return item
    }

    private func submenu(named name: String) -> (NSMenuItem, NSMenu) {
        let menu = NSMenu(title: name)
        let item = NSMenuItem()
        item.submenu = menu
        return (item, menu)
    }
}
