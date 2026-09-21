import AppKit
import PDFKit

private struct Passage: Decodable {
    let page: Int
    let rect: [CGFloat]
}

private enum SearchResponse: Decodable {
    case results([Passage], candidates: Int?, truncated: Bool)
    case failure(String)

    private enum CodingKeys: String, CodingKey {
        case ok, hits, candidates, truncated, error
    }

    init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        if try values.decode(Bool.self, forKey: .ok) {
            self = .results(
                try values.decode([Passage].self, forKey: .hits),
                candidates: try values.decodeIfPresent(Int.self, forKey: .candidates),
                truncated: try values.decode(Bool.self, forKey: .truncated)
            )
        } else {
            self = .failure(try values.decode(String.self, forKey: .error))
        }
    }
}

@MainActor
final class DocumentSearch: NSObject, NSSearchFieldDelegate {
    let view = NSView()
    private let pdfView: PDFView
    private let field = NSSearchField()
    private let status = NSTextField(labelWithString: "")
    private let mode = NSSegmentedControl()
    private let previous = NSButton()
    private let next = NSButton()
    private var height: NSLayoutConstraint?
    private var process: Process?
    private var selections: [PDFSelection] = []
    private var active = 0
    private var total: Int?
    private var truncated = false

    var hasResults: Bool { !selections.isEmpty }

    init(pdfView: PDFView) {
        self.pdfView = pdfView
        super.init()
        configure()
    }

    deinit {
        if let process, process.isRunning {
            process.terminate()
        }
    }

    func show(meaning: Bool) {
        if mode.selectedSegment != (meaning ? 1 : 0) {
            clear()
        }
        mode.selectedSegment = meaning ? 1 : 0
        height?.constant = 76
        view.isHidden = false
        view.window?.makeFirstResponder(field)
        field.selectText(nil)
        prompt()
    }

    @objc func close() {
        clear()
        height?.constant = 0
        view.isHidden = true
        view.window?.makeFirstResponder(pdfView)
    }

    func move(by offset: Int) {
        guard hasResults else { return }
        active = (active + offset + selections.count) % selections.count
        reveal()
    }

    private func configure() {
        view.translatesAutoresizingMaskIntoConstraints = false
        view.isHidden = true
        height = view.heightAnchor.constraint(equalToConstant: 0)
        height?.isActive = true

        field.placeholderString = "Find in document"
        field.setAccessibilityLabel("Find in document")
        field.sendsWholeSearchString = true
        field.sendsSearchStringImmediately = false
        field.maximumRecents = 0
        field.delegate = self
        field.target = self
        field.action = #selector(search)
        field.setContentHuggingPriority(.defaultLow, for: .horizontal)

        mode.segmentCount = 2
        mode.setLabel("Words", forSegment: 0)
        mode.setLabel("Meaning", forSegment: 1)
        mode.selectedSegment = 0
        mode.target = self
        mode.action = #selector(modeChanged)
        mode.setAccessibilityLabel("Search mode")

        let done = NSButton(title: "Done", target: self, action: #selector(close))
        done.bezelStyle = .rounded
        let queryRow = NSStackView(views: [field, mode, done])
        queryRow.spacing = 8
        queryRow.alignment = .centerY
        queryRow.translatesAutoresizingMaskIntoConstraints = false

        status.font = .systemFont(ofSize: 11)
        status.textColor = .secondaryLabelColor
        status.lineBreakMode = .byTruncatingTail
        status.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        status.setContentHuggingPriority(.defaultLow, for: .horizontal)
        configureNavigation(previous, symbol: "chevron.up", label: "Previous result", action: #selector(previousResult))
        configureNavigation(next, symbol: "chevron.down", label: "Next result", action: #selector(nextResult))
        let resultRow = NSStackView(views: [status, previous, next])
        resultRow.spacing = 8
        resultRow.alignment = .centerY
        resultRow.translatesAutoresizingMaskIntoConstraints = false

        view.addSubview(queryRow)
        view.addSubview(resultRow)
        NSLayoutConstraint.activate([
            queryRow.leadingAnchor.constraint(equalTo: view.leadingAnchor, constant: 12),
            queryRow.trailingAnchor.constraint(equalTo: view.trailingAnchor, constant: -12),
            queryRow.topAnchor.constraint(equalTo: view.topAnchor, constant: 8),
            resultRow.leadingAnchor.constraint(equalTo: queryRow.leadingAnchor),
            resultRow.trailingAnchor.constraint(equalTo: queryRow.trailingAnchor),
            resultRow.topAnchor.constraint(equalTo: queryRow.bottomAnchor, constant: 4),
        ])
    }

    private func configureNavigation(_ button: NSButton, symbol: String, label: String, action: Selector) {
        button.image = NSImage(systemSymbolName: symbol, accessibilityDescription: label)
        button.bezelStyle = .rounded
        button.toolTip = label
        button.setAccessibilityLabel(label)
        button.target = self
        button.action = action
        button.isEnabled = false
    }

    @objc private func previousResult() { move(by: -1) }
    @objc private func nextResult() { move(by: 1) }

    @objc private func modeChanged() {
        clear()
        prompt()
        view.window?.makeFirstResponder(field)
    }

    private func prompt() {
        guard !hasResults, process == nil else { return }
        message(mode.selectedSegment == 1
            ? "Press Return to rank related passages locally. Results may not match."
            : "Press Return to find these words.")
    }

    private func message(_ text: String, error: Bool = false) {
        status.stringValue = text
        status.toolTip = text
        status.textColor = error ? .systemRed : .secondaryLabelColor
        status.setAccessibilityValue(text)
    }

    private func clear() {
        if let process, process.isRunning {
            process.terminate()
        }
        process = nil
        selections = []
        active = 0
        total = nil
        truncated = false
        previous.isEnabled = false
        next.isEnabled = false
        pdfView.highlightedSelections = nil
        pdfView.clearSelection()
    }

    func controlTextDidChange(_ notification: Notification) {
        clear()
        prompt()
    }

    func control(_ control: NSControl, textView: NSTextView, doCommandBy commandSelector: Selector) -> Bool {
        guard commandSelector == #selector(NSResponder.cancelOperation(_:)) else { return false }
        close()
        return true
    }

    @objc private func search() {
        clear()
        let query = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !query.isEmpty else {
            prompt()
            return
        }
        guard let url = pdfView.document?.documentURL else {
            message("Save this document before searching it.", error: true)
            return
        }
        let executable = FileManager.default.homeDirectoryForCurrentUser.appending(path: ".local/bin/pdf-goat")
        guard FileManager.default.isExecutableFile(atPath: executable.path) else {
            message("Install the pdf-goat command-line tool to search this document.", error: true)
            return
        }

        let child = Process()
        let output = Pipe()
        let errors = Pipe()
        child.executableURL = executable
        child.arguments = ["--agent", "search", "--limit", "50"]
            + (mode.selectedSegment == 1 ? ["--meaning"] : [])
            + ["--", url.path, query]
        // A find must not spawn the CLI's parallel extraction pool. Cancelling
        // the single child also cancels all work started by this search.
        var environment = ProcessInfo.processInfo.environment
        environment["PDF_GOAT_WORKERS"] = "1"
        child.environment = environment
        child.standardOutput = output
        child.standardError = errors
        do {
            try child.run()
        } catch {
            message(error.localizedDescription, error: true)
            return
        }
        process = child
        message("Searching locally…")
        let diagnostics = Task.detached {
            errors.fileHandleForReading.readDataToEndOfFile()
        }
        Task.detached { [weak self] in
            let data = output.fileHandleForReading.readDataToEndOfFile()
            child.waitUntilExit()
            let errorData = await diagnostics.value
            await self?.finish(child, data: data, diagnostics: errorData)
        }
    }

    private func finish(_ child: Process, data: Data, diagnostics: Data) {
        guard process === child else { return }
        process = nil
        do {
            let response = try JSONDecoder().decode(SearchResponse.self, from: data)
            switch response {
            case .failure(let error):
                message(error, error: true)
            case .results(let passages, let candidates, let limited):
                guard child.terminationStatus == 0 else {
                    message("Search exited before it finished. Try again.", error: true)
                    return
                }
                total = candidates
                truncated = limited
                selections = passages.compactMap(selection)
                guard selections.count == passages.count else {
                    selections = []
                    message("The document changed or a result could not be located. Reopen it and search again.", error: true)
                    return
                }
                previous.isEnabled = hasResults
                next.isEnabled = hasResults
                reveal()
            }
        } catch {
            let detail = String(decoding: diagnostics, as: UTF8.self).trimmingCharacters(in: .whitespacesAndNewlines)
            message(detail.isEmpty ? "Search did not return a readable result: \(error.localizedDescription)" : detail, error: true)
        }
    }

    private func selection(for passage: Passage) -> PDFSelection? {
        guard passage.page > 0,
              let document = pdfView.document, passage.page <= document.pageCount,
              let page = document.page(at: passage.page - 1),
              let crop = page.pageRef?.getBoxRect(.cropBox),
              passage.rect.count == 4, passage.rect.allSatisfy(\.isFinite) else {
            return nil
        }
        let rect = passage.rect
        // CLI coordinates are crop-local, top-left, unrotated points.
        // PDFKit selections use raw PDF coordinates, even on rotated pages.
        let bounds = CGRect(x: crop.minX + rect[0], y: crop.maxY - rect[3],
                            width: rect[2] - rect[0], height: rect[3] - rect[1])
        guard bounds.width > 0, bounds.height > 0 else { return nil }
        return page.selection(for: bounds)
    }

    private func reveal() {
        guard hasResults else {
            message(mode.selectedSegment == 1 ? "No readable passages. Scanned pages need OCR first." : "No matches.")
            return
        }
        let selection = selections[active]
        selection.color = .systemYellow.withAlphaComponent(0.45)
        pdfView.highlightedSelections = [selection]
        pdfView.go(to: selection)
        let count = total.map(String.init) ?? "\(selections.count)\(truncated ? "+" : "")"
        let kind = mode.selectedSegment == 1 ? "related passages, ranked" : "matches"
        let limit = truncated ? " · showing the first \(selections.count)" : ""
        message("\(active + 1) of \(count) \(kind)\(limit)")
    }
}
