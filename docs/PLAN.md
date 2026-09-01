# Native macOS implementation plan

Status: this file defines the proposed milestone sequence. Milestone 0
measurements decide the targets and choices that are not yet proved.

## Outcome

`PDFGoat.app` is the target native interface for every included row in
[`FEATURES.md`](FEATURES.md). The target native `pdf-goat` CLI controls the
same live document. The private Python worker handles cold operations that do
not yet have a correct Apple-framework implementation.

## Milestone 0: measurement and platform proof

Build the proof before committing to a renderer, transport, storage layout, or foreign-language helper.

### Work

1. Create the Swift 6 AppKit project and a minimal `NSDocument` PDF type.
2. Add signposts for process launch, document open, first page pixels, scroll commits, search, thumbnails, save, command dispatch, job acceptance, and memory pressure.
3. Assemble the permanent S, M, L, and P corpus from real files.
4. Measure `PDFView` rendering, selection, search, annotation persistence, page operations, save, and memory behavior.
5. Prove text, button, and choice widget read, fill, create, save, and cross-reader behavior.
6. Compare one versioned JSON request and response over Apple Events and XPC. Test first-run and denied consent, no running app, multiple sessions, headless use, latency, and reply size. Select one route.
7. Prove the standalone and live dispatch boundary defined in [`SYSTEM.md`](SYSTEM.md#native-pdf-goat-cli).
8. Prove that the current `pdf-goat --agent` jobs follow the private-worker boundary in [`SYSTEM.md`](SYSTEM.md#existing-python-transformer).
9. Prove the restricted worker and operation-scoped password transport defined in [`SYSTEM.md`](SYSTEM.md#existing-python-transformer) and [`AGENT_PROTOCOL.md`](AGENT_PROTOCOL.md#access-and-safety).
10. Prove the working-copy and source-save behavior defined in [`SYSTEM.md`](SYSTEM.md#pdfgoatapp) across autosave, Versions, Save, Replace, crash, and external source edits.
11. Prove the `DocumentEngine` transaction boundary for page reorder and widget fill through standard PDFKit views, or disable those direct-editing paths.
12. Prove the signed, notarized, Hardened Runtime release bundle with its code-signed worker and helpers.
13. Record the first baseline on the M1 performance floor and the M4 Pro 120 Hz interaction lane.

### Exit gate

- The app opens every non-pathological corpus file without whole-file `Data` loading.
- First page display does not wait for indexing, OCR, thumbnails, signature validation, or attachment scans.
- The form prototype either passes Preview and Acrobat round trips or the unsupported cases move to `Prove` with exact failures.
- Exactly one local IPC route passes headless use, consent, app lifecycle, multiple-session, latency, and reply-size checks.
- The app does not invoke the Python worker until the restricted-helper test passes.
- Autosave and Versions never write the source URL; explicit Save and Replace pass crash and external-edit tests.
- Standard PDFKit views cannot bypass revisions, transactions, receipts, or undo.
- A document job performs no dependency install or network bootstrap.
- Each proposed performance budget has a reproducible benchmark and signpost trace.

## Milestone 1: native document hot path

### Work

1. Build the standard document window with toolbar, navigator, `PDFView`, and optional inspector.
2. Implement the `NSDocument` working-copy lifecycle in [`SYSTEM.md`](SYSTEM.md#pdfgoatapp), including open, autosave, Versions, validated Save and Save As, undo, window restoration, tabs, print, and full screen.
3. Add thumbnails, outline, current page, page labels, zoom modes, display modes, links, and navigation history.
4. Add visible-first thumbnail work and memory-pressure cleanup without a custom page compositor.
5. Add encrypted-document password entry with no password persistence.
6. Add accessibility labels, keyboard commands, VoiceOver order, system appearance, Increased Contrast, and Reduced Motion behavior.

### Exit gate

- The [interface verification](#interface) passes for open, page navigation, text selection, zoom, links, print, window restoration, keyboard-only use, and VoiceOver on every accepted Milestone 1 fixture.
- Open, scroll, zoom, selection, and navigation meet the accepted Milestone 0 budgets.
- Closing, crashing, reopening, and restoring never lose a saved edit or overwrite the source unexpectedly.
- UI verification uses matched screenshots and direct keyboard and VoiceOver checks on the running app.

## Milestone 2: one document engine and agent control

### Work

1. Implement the [`DocumentEngine`](SYSTEM.md#documentengine) as the mutable session owner.
2. Implement `workspace.sqlite` and the one-time standalone job-ledger migration defined in [`SYSTEM.md`](SYSTEM.md#durable-state).
3. Implement the stable identity and geometry rules in [`SYSTEM.md`](SYSTEM.md#document-identity-and-geometry).
4. Route native UI page, annotation, and form changes through the protocol transactions in [`AGENT_PROTOCOL.md`](AGENT_PROTOCOL.md#changes-and-transactions).
5. Implement the receipt and undo semantics in [`AGENT_PROTOCOL.md`](AGENT_PROTOCOL.md#receipts).
6. Replace the public launcher with the native `pdf-goat` CLI, retain the standalone verbs, add the nine live families, and move the current implementation behind `pdf-goat-worker`.
7. Implement the password transport in [`AGENT_PROTOCOL.md`](AGENT_PROTOCOL.md#access-and-safety) and remove password-on-argv worker options.
8. Implement bounded observations, cursors, reusable result sets, file-backed render artifacts, and explicit standalone or live capability modes.
9. Add page reorder, delete, rotate, insert, replace, extract, merge, split, export-copy, common annotations, and undo.
10. Add form discovery and filling for the widget types proved in Milestone 0.

### Exit gate

- UI and agent operations change one live session and increment one revision.
- A stale command cannot mutate the document.
- A multi-operation request commits once or not at all.
- Page identity survives reorder.
- Protocol acceptance scenarios 1, 2, and 5 in `AGENT_PROTOCOL.md` pass without UI automation.
- Revisions, fingerprints, receipts, and identities survive close, crash recovery, and relaunch without reuse.
- A standalone verb refuses a source owned by a live session instead of reading stale on-disk bytes.

## Milestone 3: document editing

### Work

1. Add metadata, bookmarks, links, attachments list, comments list, and annotation filtering.
2. Add text boxes, images, shapes, stamps, callouts, visual signature marks, and measurement calibration.
3. Add PDF and image export with page range, resolution, color-space, and transparency controls.
4. Add simple PDF creation from images and blank pages.
5. Add form creation only for widget cases that passed the proof gate.
6. Add dry-run previews for whole-document replacement and signed-document edits.
7. Add operation history to the native inspector with source-versus-working-copy state.

### Exit gate

- Every editor action is available from a standard menu and keyboard path.
- Preview and Acrobat reopen the saved annotations, page edits, forms, links, and metadata correctly.
- Missing fonts and lossy output produce explicit warnings before commit.
- Measurement output names its calibration, units, and uncertainty.
- Every Milestone 3 mutation is reachable from `pdf-goat`, returns a receipt, and appears in `capabilities`.

## Milestone 4: search, OCR, review, and saved records

### Work

1. Add lazy system SQLite FTS5 indexes keyed by document revision.
2. Add Vision OCR for a visible page, page range, and resumable whole-document jobs.
3. Add one canonical conversion between Vision coordinates, page boxes, page rotation, and PDF points.
4. Add searchable OCR-layer creation behind its round-trip proof.
5. Add text and visual compare through background jobs.
6. Add read aloud and page-linked audio export through system speech.
7. Add finding and job-checkpoint records to the Milestone 2 workspace store.
8. Implement the finding state transitions in [`AGENT_PROTOCOL.md`](AGENT_PROTOCOL.md#saved-workspace-records).
9. Add the thin MCP adapter over the stable native `pdf-goat` protocol.

### Exit gate

- OCR, index, and comparison work never blocks the main actor.
- Cancelling and resuming a large job loses no more than one declared checkpoint.
- Deleting all derived indexes leaves PDFs, receipts, and findings intact.
- An agent can inspect embedded and OCR text without confusing their provenance.
- An edited evidence region makes its saved finding stale instead of silently retargeting it.
- Protocol acceptance scenarios 4 and 6 pass without UI automation.

## Milestone 5: security and advanced output

### Work

1. Add redaction marking in the native app.
2. Apply true redaction through the private Python worker until a native implementation passes the same checks.
3. Verify redaction through text extraction, object inspection, raster comparison, re-OCR of every redacted page, and document reopen.
4. Add dry-run previews for annotation and form flattening.
5. Add encryption, decryption, permission flags, sanitization, compression, linearization, repair, PDF/A, JSON and XFDF form data, page boxes, headers, footers, watermarks, Bates numbering, n-up, and booklet jobs.
6. Add Scanner and Continuity Camera import through reviewed page insertion.
7. Add certificate signing and verification only after Keychain, byte-range, timestamp, chain, and later-change tests pass.
8. Add accessibility checks and the metadata edits that preserve valid output.
9. Run every transformer job through the worker boundary in [`SYSTEM.md`](SYSTEM.md#existing-python-transformer).

### Exit gate

- A failed, cancelled, or crashed advanced job leaves the live revision and source unchanged.
- Redaction verification catches recoverable target content in text, objects, or pixels.
- Editing a signed document defaults to a copy and reports signature impact.
- Security reports contain no passwords, private key material, or document content beyond explicit evidence.
- Outputs pass the cross-reader and workflow-specific validation matrix.
- Protocol acceptance scenario 3 passes without UI automation, including re-OCR of scan-derived redactions.

## Milestone 6: content editing and conversion parity

### Work

1. Characterize existing-text and image edits by PDF structure instead of promising universal Word-like editing.
2. Support the simple content-stream cases that pass glyph, font, clipping, color, transparency, and round-trip tests.
3. Keep unsupported cases read-only and explain the exact structure that blocks the edit.
4. Expose the existing local Word, Excel, PowerPoint, HTML, table, audio, and Office-to-PDF jobs through one native conversion sheet.
5. Report conversion intent, fidelity limits, source mapping, and validation artifacts.
6. Compare the cold transformer with any native or compiled replacement on correctness, size, latency, memory, and packaging.
7. Replace a transformer route only when the replacement passes the same contract, then remove the old app route.

### Exit gate

- The app never simulates a text edit by covering content with a white rectangle.
- Every supported edit class has a corpus fixture and a cross-reader round trip.
- Conversion failures return usable diagnostics and preserve the source.
- The UI presents one conversion workflow rather than dozens of disconnected tools.

## Milestone 7: scope completion

### Work

1. Complete every non-excluded row in `FEATURES.md` or record the concrete platform failure that keeps a `Prove` row closed.
2. Run every applicable section of the [verification matrix](#verification-matrix).
3. Remove obsolete app routes, duplicate command names, temporary protocol versions, and unused caches.
4. Verify command discovery and response budgets with the supported agent clients.
5. Update the system, protocol, feature ledger, and plan from measured behavior.

### Exit gate

- Every included feature has one owner and one verified execution path.
- Every implemented feature remains included in [`FEATURES.md`](FEATURES.md).
- No blocker-level data-loss, security, signature, redaction, or stale-target finding remains.
- The app meets accepted budgets on both hardware lanes or shows an explicit degraded mode on the named corpus class.
- Every included mutating capability is reachable from `pdf-goat`, returns a receipt, and appears in `capabilities`.

## Performance corpus

Use fixed files so weekly numbers remain comparable.

| Class | Definition |
| --- | --- |
| S | At most 10 MB and 100 pages, with digital text |
| M | At most 100 MB and 1,500 pages, with mixed text and images |
| L | At most 2 GB and 10,000 pages, primarily scanned |
| P | One distinct edge: at least 100,000 vector operations on one page, malformed cross-reference data, 10,000 annotations, or difficult JBIG2/CCITT scans |

S, M, L, and P are fixture classes, not an automatic classifier. Their
definitions overlap. The corpus manifest assigns each fixture to one class and
records its stable fixture name, SHA-256 digest, byte count, page count, and
relevant edge.

The corpus covers:

- Small text PDFs, thousand-page text PDFs, and large scanned PDFs.
- Mixed page sizes and rotations, missing and embedded fonts, layers, and
  unusual page boxes.
- Encrypted and permission-restricted files, signed files, text, button, and
  choice forms, annotations, and attachments.
- Damaged and nonconforming files.

Record first-page latency, scroll frame time, peak memory, search latency, OCR
throughput, save time, output validity, and interoperability. Add a fixture
only when it exposes a distinct failure mode.

The hardware lanes are:

- M1 with 8 GB RAM as the CPU and memory floor.
- M4 Pro with a 120 Hz display as the high-refresh interaction lane.

## Proposed performance budgets

These numbers are targets until Milestone 0 measures them. Change a target only with a saved signpost trace, corpus file, and user-visible reason.

| Contract | Proposed target |
| --- | --- |
| Cold launch to first window frame | At most 400 ms p50 and 700 ms p95 on the M1 floor |
| Open to first page pixels | S: 250 ms warm p50 and 600 ms p95. L: 1 second p95 with no full-file copy |
| Scroll interaction | Main-thread commit at most 4 ms p95 on the 120 Hz lane, with no repeated dropped-frame sequence on S or M |
| Find | First visible hit within 100 ms p95 on indexed pages; repeated FTS query within 10 ms p95 |
| Background text index | 1,000 text pages within 30 seconds at utility priority, with a checkpoint at least every 256 pages or 2 seconds |
| Visible-page OCR | Accurate mode within 1 second p95, fast mode within 250 ms p95, with zero main-thread OCR work |
| Memory | Physical footprint at most 300 MB p95 after 5 minutes on M; PDF Goat-owned caches release promptly on memory pressure |
| Pathological degraded mode | Visible progress or a bounded failure within 500 ms, with cancellation and no silent interaction stall |
| Background priority | All non-visible work runs at utility priority or lower |
| Bounded live `observe` round trip | At most 50 ms p95 on the M1 floor with the selected IPC route |
| Default command reply | At most 64 KiB; larger text, images, and diffs return file paths and digests |
| Long-job acceptance | A validated job ID within 100 ms p95 before background work begins |
| Working-copy save | S within 500 ms p95 and M within 2 seconds p95; larger saves return observable progress within 100 ms |

## Performance gate definitions

- **Responsive UI:** background work must preserve the accepted scroll budget.
  No main-thread stall may exceed 100 ms. Input-to-visible-response latency
  must remain at or below 100 ms p95.
- **Memory-pressure release:** PDF Goat-owned cache references and non-visible
  work must be released within 100 ms of the memory-pressure signpost. The
  physical footprint must return to at most 300 MB p95 within the existing
  5-minute M-class window. Record delayed allocator and PDFKit release
  separately.
- **Dropped-frame sequence:** `repeated` means at least two consecutive dropped
  display frames on the M4 Pro 120 Hz lane.
- **Accurate OCR:** use a fixed OCR fixture set with stable SHA-256 digests,
  reference Unicode text, and page geometry. Report character error rate. The
  accepted Milestone 0 baseline sets the maximum error rate before the
  1-second p95 latency target can pass.

Before an A/B performance experiment, declare one primary end-to-end metric and
the minimum useful effect. Interleave like-for-like runs on the same hardware,
corpus, app configuration, and readiness detector. Accept a performance claim
only when the paired confidence interval stays on the winning side and its
lower bound clears the declared effect. Do not report p95 from a sample too
small to estimate it.

## Escalation gates

`PDFGoat.app` remains a Swift/AppKit application with PDFKit as its production
renderer. No current measurement supports a renderer replacement or a rewrite
of app-owned code in Objective-C, Objective-C++, C, C++, Rust, or Zig.

### Custom renderer gate

Do not build a custom tile compositor while `PDFView` meets the accepted scroll and render budgets. Reopen this decision only when two corpus files miss p95 in two consecutive weekly runs and Instruments assigns the miss to PDFView rather than PDF Goat code.

A renderer experiment must measure the aggregate footprint of the app and any
helper, count file and raster copies, pass the same accessibility, selection,
form, annotation, color, malformed-file, signing, and notarization checks, and
retain a clean rollback. Record the exact candidate version, build flags, and
license before the experiment. The primary renderer sources are listed in
[`SYSTEM.md`](SYSTEM.md#sources).

### Third-party parser gate

If a third-party C, C++, Rust, Zig, or other unsafe parser begins reading untrusted PDF bytes, move it to a separate XPC service with a tighter sandbox before it enters the product. The process boundary owns isolation. The implementation language does not.

The existing Python transformer already runs out of process. App integration also requires the restricted-helper gate, because process separation alone does not limit file access. It continues to receive copies and produce new files.

### Rust gate

Rust enters only if all five conditions hold:

1. An accepted p95 budget misses by at least 50 percent on two corpus files in two consecutive weekly runs on the M1 floor.
2. Instruments assigns at least 60 percent of the missed time to PDF Goat code rather than Apple frameworks.
3. One documented algorithmic Swift fix still leaves a miss of at least 25 percent.
4. A two-week-or-shorter Rust spike behind the existing boundary improves the same end-to-end metric by at least two times and passes the same tests.
5. The spike introduces no unisolated third-party parsing of untrusted bytes.

If condition 2 fails, a language rewrite cannot reach the bottleneck. The decision becomes whether to replace an Apple engine, with separate fidelity, security, size, and licensing review.

Zig has no planned role. It adds another toolchain without a current measured problem or a stronger safety case.

The same attribution requirement applies to Objective-C, Objective-C++, C, and
C++. A language change can own only a measured hot loop in PDF Goat code. It
does not replace the separate renderer and parser gates.

## Verification matrix

### Observable behavior

Test public document operations and protocol responses. Do not test implementation call order. One focused test should protect each nontrivial invariant or realistic boundary.

### Interoperability

For changed PDF structures, reopen the output in:

- PDFGoat.app.
- Apple Preview.
- Adobe Acrobat when installed.
- PDFgear when installed.

Compare visible output, page structure, annotations, forms, signatures, and extracted text as applicable.

### Data safety

Exercise:

- External file changes during an agent session.
- Save failure and low-disk conditions.
- App crash during autosave.
- Transformer crash and cancellation.
- Stale revisions and result IDs.
- Signed and encrypted files.

### Security

Use a distinct check for redaction, sanitization, encryption, certificate signing, attachments, embedded scripts, and malformed input. Never accept a visual-only redaction test.

### Severity

A blocker is a reproducible finding that can lose user work, overwrite the
source without the required action, disclose protected content, report an
invalid signature as valid, leave recoverable redacted content, target stale
content, or prevent a supported output from reopening.

### Performance

Run weekly p50 and p95 measurements with `os_signpost` and Instruments. Keep traces from failed runs. A performance fix that adds a subsystem must show the measured win and retain a direct revert path.

### Interface

Run the actual app. Verify the golden path, keyboard-only use, VoiceOver, Increased Contrast, Reduced Motion, light and dark appearance, large documents, empty states, job failure, and destructive preview. Keep matched screenshots for product UI changes.

## Plan maintenance

[`FEATURES.md`](FEATURES.md) owns scope. [`SYSTEM.md`](SYSTEM.md) owns
architecture and invariants. [`AGENT_PROTOCOL.md`](AGENT_PROTOCOL.md) owns wire
semantics. This file owns milestone order, performance budgets, and exit gates.

Keep each rule in its owner document. Other documents link to that rule and
record only the milestone status that depends on it.
