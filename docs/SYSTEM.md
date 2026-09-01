# PDF Goat system design

Status: this document defines the target native architecture.
[`README.md`](../README.md) describes the current Python CLI and read-only
native viewer.

## Purpose

The target macOS system has two clients:

- A native document app for people.
- A versioned command interface for agents working on the same open document.

The app covers the local workflows in the [feature ledger](FEATURES.md). That
ledger also owns product exclusions.

## Product contract

1. Swift 6, AppKit, and PDFKit own the interactive path. Core Graphics, Vision, Security, and Accelerate are used where measurement supports them.
2. One `DocumentEngine` owns every mutable open-document session. The UI and agent issue commands to it. They do not keep competing document state.
3. Opening, rendering, scrolling, selecting, searching, and interactive annotation never start Python, Rust, Zig, a web view, or a network request.
4. Agent changes apply to the current working copy. They never overwrite the source file in the background.
5. Every mutation has a document revision precondition, one transaction, native undo, and a structured receipt.
6. Every saved finding points to document evidence. Derived indexes remain disposable.
7. The target system is local. The [product exclusions](FEATURES.md#product-exclusions) own network services and other excluded product scope.
8. The release target is a Developer ID-signed and notarized macOS app with Hardened Runtime. App-integrated workers and helpers are bundled and code-signed. A worker capability does not ship until its declared-file-access restriction passes on the release build.

## System shape

The native app, command-line control, and MCP adapter meet at one command model.

```
+------------------ [ SYSTEM ] --------------------+
|                                                  |
| pdf-goat                                         |
| ├─ PDFGoat.app                                   |
| │  ├─ appkit + pdfview  human                    |
| │  └─ document-engine   state                    |
| └─ agent-tools                                   |
|    └─ pdf-goat + mcp    control                 |
|                                                  |
+--------------------------------------------------+
```

In the target architecture, the current Python implementation runs as the private out-of-process `pdf-goat-worker` for advanced operations that Apple frameworks do not support. It does not own a session. `DocumentEngine` gives it an immutable input snapshot, receives a new candidate PDF, validates that output, and imports it as one transaction.

## Linked abstraction levels

| Level | Owns | Can depend on |
| --- | --- | --- |
| System frameworks | PDF bytes, rendering, file coordination, OCR, cryptography | Apple frameworks and the existing transformer process |
| Document graph | Stable document, page, annotation, form, region, and result identities | System frameworks |
| Operations | Observations, commands, revisions, transactions, jobs, receipts | Document graph |
| Workflows | Annotate, organize, OCR, redact, sign, compare, convert, and verify | Operations |
| Surfaces | AppKit UI, native `pdf-goat` CLI, MCP adapter | Workflows |

Dependencies point down this table. A surface never edits `PDFDocument` directly. A workflow never parses UI state. A transformer never writes into a live source file.

## Component ownership

### `PDFGoat.app`

Use an AppKit document app. `NSDocument` owns open, save, autosave, versions,
file coordination, and undo integration. Standard AppKit window and menu
behavior takes priority over custom chrome.

The `NSDocument` file is an app-managed working copy. Autosave and Versions
write that working copy, never the source URL. Save and Replace validate a
snapshot and atomically replace the source only after an explicit human action.

The document window uses:

- `NSToolbar` for current document actions.
- `NSSplitViewController` for a left navigator, document canvas, and optional right inspector.
- `PDFView` for the canvas.
- `PDFThumbnailView` and `NSOutlineView` for pages, outline, search results, attachments, and comments.
- System fonts, semantic colors, SF Symbols, native focus rings, and system appearance.

`PDFView`, `PDFThumbnailView`, and form controls must not create an observable edit outside `DocumentEngine`. Keep a direct-editing path read-only, capture its complete edit as one engine transaction before any observer can read it, or replace that path.

Agent activity appears in the same operation history and selection model as
human activity.

### `DocumentEngine`

One engine exists per open document. It owns:

- The live `PDFDocument`.
- The current revision.
- Stable object identities.
- The working copy and source fingerprint.
- Selection and named result sets.
- Transactions, jobs, and receipts.
- Cache invalidation.

UI work stays on the main actor. OCR, extraction, comparison, and conversion run outside the main actor with copied value data or immutable file snapshots. PDFKit objects do not cross concurrency boundaries without an explicit verified contract.

### Native `pdf-goat` CLI

The target public `pdf-goat` command is a small Swift command-line target with two explicit profiles. Existing file-in file-out verbs run without the app through a transient `DocumentEngine`. Live-document families target a named app session. Both profiles use the same command and receipt types. The current Python launcher runs behind the private `pdf-goat-worker` name, while existing file verbs keep their names and JSON contracts.

Dispatch never guesses. A request with a session and document ID uses the live profile. A request with file paths and no session ID uses the standalone profile. A request that identifies both fails. Before standalone work opens a source, it checks the workspace for a live session with that source fingerprint and fails with `source_changed` instead of reading stale on-disk bytes.

Live commands use the one local IPC route selected in Milestone 0. Large text,
images, and diffs go to files in the workspace. The reply contains a path and
metadata instead of base64 data.

### MCP adapter

The MCP adapter is a thin local adapter over the native `pdf-goat` CLI. It does not contain PDF logic. It exposes a small set of workflow tools and discloses detailed command schemas on demand. This avoids loading dozens of feature-specific tool descriptions into every agent turn.

### Existing Python transformer

The current Python implementation covers merge, split, page operations, annotation, forms, security, conversion, optimization, accessibility, comparison, repair, and extraction. The target system uses it as the private worker for cold advanced jobs until native code covers an operation.

Rules for this boundary:

1. Launch it only for an explicit job.
2. Give it a read-only snapshot or temporary copy.
3. Run every worker job through a restricted helper that can access only the declared temporary input and output. Process separation without reduced file access is crash containment, not a security sandbox.
4. Require a new output path.
5. Capture structured output and process termination.
6. Validate that the output opens, has the expected page count, and satisfies the workflow-specific postcondition.
7. Import the output through `DocumentEngine` as one undoable replacement.
8. Do not retry automatically after a deterministic failure.
9. Install or update the bundled worker outside document jobs. A missing or damaged worker environment fails fast with `worker_failed`; a job never downloads or installs code.

When a native implementation replaces an app route to the transformer, remove the old app route in the same change.

## Document identity and geometry

Page indexes are display positions, not identities. Reordering a page must not change its identity.

A workspace assigns:

- One random document ID.
- One stable page ID per page.
- Annotation IDs stored in the annotation name field when PDFKit supports it, with a sidecar mapping for existing annotations.
- Revision-bound IDs for search results, extracted blocks, and temporary regions.

Every geometric result states:

- Page ID.
- PDF page box.
- Rotation.
- Rectangle in PDF points.
- Origin and axis direction.
- Render scale when pixels are involved.

Commands that use a revision-bound object fail with `stale_reference` after the owning page changes. They return the current revision and the smallest observation needed to recover.

After an external edit or full-document replacement, reconcile page and object IDs only when identity or content evidence gives one unambiguous match. Otherwise assign new IDs, return an explicit old-to-new map with unresolved entries, and mark affected findings and selections stale. Never guess from page position alone.

## State and storage

The PDF remains the source of truth for document content. PDF Goat stores supporting state under `~/Library/Application Support/PDFGoat/`.

### Durable state

`workspace.sqlite` stores:

- Document identities and security-scoped file bookmarks.
- Operation receipts.
- Named selections and bookmarks.
- Agent and human findings with evidence, author, confidence, and source revision.
- Long-running job records.

The current revision and working-copy fingerprint persist together. A revision number is never reused for different bytes after close, crash, recovery, or relaunch.

`workspace.sqlite` is the target job and receipt store. Milestone 2 migrates the
current standalone job ledger once. The private worker returns results to that
store and does not mirror the ledger.

A finding is not silently rewritten when the PDF changes. It becomes `stale` if its cited page or region changes, and it keeps the old evidence for review.

### Disposable state

Each document index stores extracted text, OCR text, block geometry, thumbnails, and full-text search data keyed by document revision. Deleting an index must not delete a note, receipt, annotation, or PDF change.

Automatic accumulation stops at derived data and operation receipts. Durable
findings require an explicit `remember` action. This keeps derived caches
separate from durable findings.

## Observation and action loop

[`AGENT_PROTOCOL.md`](AGENT_PROTOCOL.md) owns the command sequence and wire semantics. At the architecture level, every structure or pixel observation names one revision, the engine validates every operation before applying the first, and verification reads persisted output through a path independent of the mutation. The agent requests pixels only when structure is insufficient.

## Human interaction model

- The left sidebar switches between thumbnails, outline, search, bookmarks, attachments, and comments.
- The right inspector shows the selected annotation, form field, page, job, or operation. It stays hidden when there is no useful selection.
- Page organization uses direct manipulation in the thumbnail sidebar with keyboard equivalents and an undoable transaction.
- Search results show a short text match and page location. Selecting a result navigates without changing zoom unexpectedly.
- Destructive tools show a working-copy banner, affected page count, and preview. They do not use repeated modal confirmations.
- Agent changes animate only as ordinary selection and operation-state changes. Reduced Motion removes nonessential animation.
- Every control remains available from menus for keyboard access and discoverability.

Do not create a custom visual design system before a native control fails a measured need. Use standard macOS spacing, typography, appearance, accessibility, toolbar customization, window restoration, drag and drop, Quick Look handoff, and Services where applicable.

## Native performance architecture

The native app remains a Swift/AppKit application with PDFKit as the production
renderer. Current measurements do not support a renderer replacement or a
rewrite of app-owned code in Objective-C, Objective-C++, C, C++, Rust, or Zig.
The [escalation gates](PLAN.md#escalation-gates) own any future language or
renderer experiment.

Specific rules:

- Show the first page before OCR, full-document search indexing, thumbnail completion, attachment inspection, or signature validation.
- Render the visible page range and a small look-ahead range. Cancel work that leaves that range.
- Do not duplicate PDFView's tile cache. Use cost-limited `NSCache` only for PDF Goat-owned thumbnails, crops, and OCR previews. Release those caches on memory pressure.
- Index pages incrementally and persist progress by revision.
- Run OCR page by page with bounded concurrency. Keep one canonical coordinate conversion path.
- Return a job ID quickly for long work. Progress and cancellation are observable.
- Avoid full-document `Data` copies. Prefer file URLs, immutable snapshots, mapped reads where the underlying API supports them, and streaming output.
- Keep the document canvas in AppKit. Do not wrap the hot canvas in a new SwiftUI abstraction.

The [escalation gates](PLAN.md#escalation-gates) own the conditions for a
compiled helper, foreign parser, or replacement renderer.

## Failure model and recovery

| Small failures that can combine | Bad result | Defense layers |
| --- | --- | --- |
| External file change, stale agent revision, background overwrite | User work is lost | `NSFilePresenter`, source fingerprint, revision precondition, working copy, atomic replacement, native versions |
| OCR box error, rotated or cropped page, applied redaction | Sensitive content remains | One geometry conversion path, visual preview, source-text check, post-redaction extraction and render verification |
| Existing signature, unnoticed content edit, in-place save | A signed document becomes invalid without warning | Signature-state gate, save-copy default, receipt warning, independent signature verification |
| Stale text index, changed page content, reused result ID | Edit targets the wrong text | Revision-keyed index, revision-bound result IDs, `stale_reference` error |
| Malformed PDF, transformer crash, automatic retry | Resource exhaustion or partial output | Out-of-process job, temporary output, one failure receipt, no automatic retry |
| Memory pressure, eager thumbnail or OCR work, long document | Scrolling stalls or the app is killed | Visible-first work, bounded concurrency, cancellation, disposable cost-limited caches |
| Dry run, changed targets, later apply | Reviewed targets differ from changed targets | Revision precondition and stateless dry-run digest over canonical operations and target IDs |
| Long job, live edit, late worker output | Worker output overwrites newer work | Job input revision, import through `change`, `stale_revision`, preserved temporary output |
| Standalone CLI, same source open in app, stale on-disk bytes | Output omits live edits | Workspace live-session lookup, source fingerprint, `source_changed` |
| Whole-document replacement, ambiguous identity match, saved evidence | Finding points at the wrong content | Explicit page and object identity map, unresolved entries, stale findings and selections |
| IPC consent denial, app unavailable, silent transport fallback | Agent hangs or targets the wrong process | One shipped IPC route, bounded `session_unavailable` error, headless acceptance test |

The app must show source-versus-working-copy state, current revision, active jobs, warnings, and a direct undo or discard path. A failed advanced job leaves the live session unchanged.

## Performance corpus

The [implementation plan](PLAN.md#performance-corpus) owns the fixed corpus,
measurements, and performance gates.

## Sources

- [Apple PDFKit](https://developer.apple.com/documentation/pdfkit)
- [Apple PDFView](https://developer.apple.com/documentation/pdfkit/pdfview)
- [Apple PDF widgets](https://developer.apple.com/documentation/pdfkit/pdf-widgets)
- [Adding widgets to a PDF document](https://developer.apple.com/documentation/pdfkit/adding-widgets-to-a-pdf-document)
- [Apple NSDocument](https://developer.apple.com/documentation/appkit/nsdocument)
- [Apple Vision text recognition](https://developer.apple.com/documentation/vision/vnrecognizetextrequest)
- [Adobe Acrobat](https://www.adobe.com/acrobat.html)
- [PDFgear for Mac](https://www.pdfgear.com/pdfgear-for-mac/)
- [Why PDFgear is free](https://www.pdfgear.com/insights/is-pdfgear-free.htm)
- [PDFium BSD 3-Clause license](https://pdfium.googlesource.com/pdfium/+/main/LICENSE)
- [MuPDF source and license](https://github.com/ArtifexSoftware/mupdf)
- [MuPDF releases](https://mupdf.com/releases)
- [Poppler project](https://poppler.freedesktop.org/)
- [Poppler source and license](https://gitlab.freedesktop.org/poppler/poppler)
- Existing `pdf-goat` README and command help in this repository
