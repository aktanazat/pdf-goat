# Feature ledger

Status: this is the target feature ledger. Class and milestone identify planned
ownership and delivery order. They do not mean that a feature is implemented.
[`README.md`](../README.md) describes the current Python CLI and native viewer.

## Scope rule

A feature belongs in PDF Goat when it does at least one of these jobs:

- Makes a frequent local PDF task faster.
- Prevents data loss, privacy failure, or invalid output.
- Gives an agent accurate observation or control.
- Preserves a specialized workflow already implemented in `pdf-goat` with low product cost.

A matching competitor feature does not establish scope. Cloud infrastructure,
account systems, repeated AI interfaces, and specialist formats without a
current workflow stay out.

## Competitor facts

Adobe Acrobat Pro currently groups desktop work around editing, page
organization, OCR, forms, conversion, comparison, redaction, protection,
accessibility, signing, and guided batch actions.

PDFgear has no Pro or Premium tier as of August 31, 2026. Its official material
says the desktop editor remains free and that charging for some future advanced
options is still under consideration. This ledger uses the Mac features listed
in PDFgear's current official material.

The current `pdf-goat` CLI covers document structure, annotations, forms,
security, conversion, optimization, accessibility, comparison, repair, and
extraction. The target native app uses the current CLI through the private
worker when native coverage is not yet available.

## Delivery classes

| Class | Meaning |
| --- | --- |
| Native | Swift and Apple frameworks own the interactive or frequent path |
| Extended | Private `pdf-goat-worker` owns a cold local job while native coverage grows |
| Prove | A correctness or compatibility prototype must pass before the feature can mutate user files |
| Excluded | Intentionally outside the product |

`Prove` means the prototype must pass its completion condition before the
feature can change a user file.

## Viewing and navigation

| Capability | Class | Milestone | Completion condition |
| --- | --- | --- | --- |
| Open local and encrypted PDFs | Native | 1 | Opens through `NSDocument`, keeps passwords out of logs and durable state, and shows first-page pixels before background work |
| Multiple windows, tabs, recents, restoration | Native | 1 | Uses standard macOS document behavior and restores position and zoom |
| Continuous, single-page, and two-up modes | Native | 1 | Mode changes preserve current page and selection |
| Fit width, fit page, actual size, zoom, and view rotation | Native | 1 | Menu, toolbar, gesture, and keyboard paths agree |
| Full screen and presentation | Native | 1 | Hides editing chrome and keeps navigation accessible |
| Thumbnails and page labels | Native | 1 | Loads visible thumbnails first and updates after page transactions |
| Document outline and bookmarks | Native | 1 | Navigate, add, rename, reorder, and remove without changing page identity |
| Search results sidebar | Native | 2 | Shows bounded excerpts, page IDs, and geometry with keyboard navigation |
| Text selection and copy | Native | 1 | Preserves Unicode text where the PDF exposes it |
| Exact, case, whole-word, and regular-expression search | Native | 2 | Results are revision-bound and reusable by later commands |
| Back and forward navigation history | Native | 1 | Link and search navigation can return to the prior view |
| Internal and external links | Native | 2 | Inspects destinations before opening external URLs |
| Attachments and portfolios as a list | Native | 3 | Lists and extracts attachments without executing content |
| Comments and annotations sidebar | Native | 3 | Filters by page, type, author, and status |
| Read aloud | Native | 4 | Uses system speech and follows reading order or reports low confidence |
| Print with the system panel | Native | 1 | Uses AppKit print settings and preserves page boxes |
| Dark application chrome | Native | 1 | Follows system appearance and does not invert document colors by default |

## Page organization and document creation

| Capability | Class | Milestone | Completion condition |
| --- | --- | --- | --- |
| Merge PDFs | Native | 2 | Preserves page order, boxes, rotation, and page identity in the new document |
| Split by pages, ranges, or file size | Native | 2 | Produces independently valid files and a receipt for each output |
| Extract pages | Native | 2 | Exports selected page IDs without changing the source |
| Insert and replace pages | Native | 2 | Validates source and destination page boxes before commit |
| Duplicate, reorder, and delete pages | Native | 2 | One drag or command becomes one undoable transaction |
| Rotate pages | Native | 2 | Changes page rotation, not only the current view |
| Crop pages | Native | 3 | Previews crop box changes and preserves recoverable page content until flatten or export |
| Scale page content and size | Extended | 5 | Produces expected media and crop boxes and passes cross-reader checks |
| Create PDF from images | Native | 3 | Applies orientation and color profile correctly |
| Add blank pages | Native | 3 | Uses explicit page size and insertion point |
| Header, footer, and page numbers | Extended | 5 | Supports page ranges, margins, font, alignment, and preview |
| Bates numbering | Extended | 5 | Enforces unique sequence, prefix, suffix, and no accidental reuse |
| Watermark and background | Extended | 5 | Supports text or PDF source, opacity, placement, and page range |
| Overlay and stamp one PDF over another | Extended | 5 | Normalizes page boxes and produces a preview |
| N-up and booklet imposition | Extended | 5 | Verifies order, orientation, creep assumptions, and printable boxes |
| Page box editor | Extended | 5 | Reads and writes media, crop, trim, bleed, and art boxes without silent coercion |
| Page labels | Native | 3 | Separates display labels from page indexes |
| Metadata read, set, and strip | Native | 3 | Handles standard and custom keys and makes each removed or changed key observable |
| Add, edit, and remove links | Native | 3 | Supports internal destinations and inspected external URLs |
| Add, extract, and remove attachments | Extended | 5 | Never executes an attachment and preserves file names safely |

## Annotation and review

| Capability | Class | Milestone | Completion condition |
| --- | --- | --- | --- |
| Highlight, underline, and strikeout | Native | 2 | Stores quadrilateral text geometry and interoperates with Preview and Acrobat |
| Area highlight | Native | 2 | Uses explicit page rectangles when text geometry is unavailable |
| Sticky note and free text | Native | 2 | Preserves author, contents, appearance, and bounds |
| Rectangle, circle, line, arrow, and polygon | Native | 2 | Supports stroke, fill, width, and native selection handles |
| Ink and freehand drawing | Native | 2 | Coalesces points without losing pressure-independent geometry |
| Stamp and callout | Native | 3 | Supports a small native set and user images without a template marketplace |
| Annotation move, resize, edit, and delete | Native | 2 | Each gesture is one undo group |
| Annotation list, filter, and export | Native | 3 | Exports stable IDs, page IDs, geometry, author, contents, and status |
| Annotation flatten | Prove | 5 | Rendering and extraction prove that appearance is preserved in other readers |
| Visual signature mark | Native | 3 | Draw, type, or import an image and label it as an electronic mark, not a certificate signature |
| Distance, perimeter, and area measurement | Native | 3 | Keeps a per-document calibration value and reports units and uncertainty |
| Text comparison | Extended | 4 | Reports additions, removals, and page mapping with bounded context |
| Visual page comparison | Extended | 4 | Produces aligned diff images and a changed-region list |
| Side-by-side synchronized review | Native | 4 | Links page and zoom only when page mapping is known |

## Forms and signatures

Apple PDFKit supports text, button, and choice widgets. Full Acrobat form parity, XFA, and certificate workflows need separate proof.

| Capability | Class | Milestone | Completion condition |
| --- | --- | --- | --- |
| Detect and list form widgets | Native | 2 | Reports field name, type, page, bounds, value, options, and flags |
| Fill text, button, and choice widgets | Native | 2 | Values persist and render in Preview and Acrobat |
| Create text, button, and choice widgets | Prove | 3 | Appearance streams and field hierarchy survive cross-reader round trips |
| Fill a noninteractive form with text, marks, images, and signatures | Native | 3 | Added objects remain editable until flatten or export |
| Import and export field data | Extended | 5 | Supports JSON and XFDF with explicit field mismatch errors |
| Flatten forms | Prove | 5 | Values remain visible and no editable field survives |
| Certificate signing | Prove | 5 | Uses Keychain identities, preserves the signed byte range, and passes independent validation |
| Signature verification | Prove | 5 | Reports signer, chain, timestamp, covered revision, trust result, and later changes |
| Request signatures from remote people | Excluded | None | Requires accounts, identity, email delivery, tracking, and cloud audit infrastructure |
| XFA forms | Excluded | None | Requires a separate runtime and has no current user workflow |

## Content editing, OCR, and extraction

| Capability | Class | Milestone | Completion condition |
| --- | --- | --- | --- |
| Add text | Native | 3 | Embeds or selects a compatible font and warns before substitution |
| Edit simple existing text runs | Prove | 6 | Preserves glyph positioning, encoding, and surrounding content on the corpus |
| Find and replace simple text runs | Prove | 6 | Requires expected match count and proves removed text is absent |
| Add, move, resize, replace, and delete images | Prove | 6 | Preserves clipping, masks, color space, and transparency |
| Add and edit shapes and links | Native | 3 | Uses PDF objects that remain editable where supported |
| OCR a visible page | Native | 4 | Vision returns text and geometry without blocking the main actor |
| OCR a full document | Native | 4 | Runs as a resumable, cancellable page job with language choice |
| Add a searchable OCR layer | Prove | 4 | Text aligns with pixels and survives round trips without covering source scans |
| Deskew, rotate, and clean scan pages | Prove | 4 | Preview shows the exact raster change before replacement |
| Extract text with page and block geometry | Native | 3 | Distinguishes embedded text from OCR and reports reading-order confidence |
| Extract Markdown and layout-aware plain text | Extended | 4 | Preserves headings, lists, columns, and source page links where detected |
| Extract tables to CSV | Extended | 5 | Reports merged cells, confidence, and source rectangles instead of claiming lossless output |
| Extract images, fonts, attachments, bookmarks, and links | Extended | 5 | Writes assets safely and returns provenance |
| PDF to PNG, JPEG, and TIFF | Native | 3 | Supports page range, resolution, color space, and transparency where possible |
| Images to PDF | Native | 3 | Produces explicit page sizes and orientation |
| PDF to HTML | Extended | 6 | States whether output targets visual fidelity or semantic structure |
| PDF to Word, Excel, and PowerPoint | Extended | 6 | Uses the existing local transformer and reports fidelity limits |
| Office documents to PDF | Extended | 6 | Remains an explicit cold job and does not add an office suite to the app |
| PDF/A conversion and validation | Extended | 5 | Uses an explicit target conformance level and reports an independent validation result |
| Audio export | Extended | 4 | Uses system speech and saves page-linked narration metadata |
| Scanner and Continuity Camera import | Native | 5 | Uses system capture and inserts reviewed pages into the working copy |

## Security, quality, and standards

| Capability | Class | Milestone | Completion condition |
| --- | --- | --- | --- |
| Encrypt and decrypt | Extended | 5 | Uses modern PDF encryption, validates reopen, and never overwrites the only copy |
| Print, copy, and modify permissions | Extended | 5 | Reads and writes permission flags without overstating enforcement |
| Mark redaction regions | Native | 4 | Supports text results, regions, and expected target counts |
| Apply true text and image redaction | Prove | 5 | Removes underlying content and verifies extraction, objects, re-OCR, and pixels |
| Sanitize hidden data | Extended | 5 | Removes selected metadata, scripts, attachments, hidden objects, and prior revisions with a report |
| Detect embedded JavaScript | Extended | 5 | Reports and removes it; never executes it |
| Compress with quality choices | Extended | 5 | Shows before and after size and validates visual and structural output |
| Reduce to a target size | Extended | 5 | Reports when the target cannot be reached within the quality floor |
| Linearize for fast web access | Extended | 5 | Validates linearization without adding a web product |
| Repair damaged PDFs | Extended | 5 | Writes a copy, reports repaired structures, and preserves the original |
| Text and visual diff | Extended | 4 | Produces machine results and inspectable artifacts |
| Accessibility check | Extended | 5 | Reports title, language, tags, reading order, headings, alt text, form labels, and contrast where measurable |
| Accessibility metadata editing | Prove | 6 | Changes tags and reading order only when the output passes a cross-reader check |
| Advanced print preflight, separations, trapping, and press profiles | Excluded | None | Specialist print-production work would require a separate product and engine |

## Agent and automation features

| Capability | Class | Milestone | Completion condition |
| --- | --- | --- | --- |
| Machine JSON input and output | Native | 2 | Every command has a versioned schema and stable error codes |
| Capability discovery | Native | 2 | Detailed schemas and standalone or live mode load by family on demand |
| Stable document, page, and object IDs | Native | 2 | Reorder and edits preserve identity or return an explicit replacement map |
| Revision preconditions | Native | 2 | A stale command cannot change the working copy |
| Bounded observations and cursors | Native | 2 | No default command dumps a whole large document |
| Region rendering with overlays | Native | 2 | Artifact metadata maps pixels back to PDF coordinates and object IDs |
| Dry runs for high-risk changes | Native | 3 | Returns exact targets and a stateless digest without creating mutable duplicate state |
| Atomic transactions and native undo | Native | 2 | Multi-operation changes commit once or not at all; undo is session-scoped and revisioned |
| Structured receipts | Native | 2 | Reports revisions, changed objects, outputs, identity maps, warnings, undo, and verification |
| Observable jobs, cancellation, and resume | Native | 4 | Long work reports real progress, durable checkpoints, and safe cancellation boundaries |
| Named selections and result sets | Native | 2 | Agents reuse server-side targets without resending large payloads |
| Evidence-linked findings | Native | 4 | Stale evidence remains visible after source changes |
| Built-in cross-document batch workflow engine | Excluded | None | Agents and scripts compose one-file commands without duplicate orchestration or false cross-file atomicity |
| MCP adapter | Native | 4 | Thin mapping over the native `pdf-goat` protocol with no PDF logic or duplicate state |
| Generic built-in AI chat, summarizer, or copilot | Excluded | None | The external agent already owns reasoning and natural-language control |

## Product exclusions

These stay out unless the user supplies a new concrete workflow that changes the cost:

- Accounts, subscriptions, ads, telemetry, and upsell surfaces.
- Cloud file sync and automatic uploads.
- Shared cloud review rooms and PDF Spaces.
- Remote signature request and tracking services.
- Team administration, SSO, role dashboards, and enterprise audit products.
- A second AI assistant, chat sidebar, podcast generator, presentation generator, or image generator.
- Web, Windows, Android, and iOS versions.
- A plug-in marketplace or public extension API.
- Template stores and generic document CRM features.
- Embedded JavaScript execution.
- XFA, 3D, multimedia, and geospatial PDF authoring.
- Automatic summaries, inferred user preferences, embeddings, and a general memory store unless a concrete workflow shows that explicit findings and full-text search are insufficient.
- Custom themes, custom title bars, decorative motion, and nonstandard controls.

## Sources

- [Adobe Acrobat product and plan features](https://www.adobe.com/acrobat.html)
- [Adobe guided actions](https://helpx.adobe.com/acrobat/using/action-wizard-acrobat-pro.html)
- [Adobe redaction](https://helpx.adobe.com/acrobat/desktop/protect-documents/redact-pdfs/redact.html)
- [PDFgear for Mac](https://www.pdfgear.com/pdfgear-for-mac/)
- [PDFgear current release notes](https://www.pdfgear.com/whats-new/)
- [PDFgear pricing statement](https://www.pdfgear.com/insights/is-pdfgear-free.htm)
- [Apple PDF widgets](https://developer.apple.com/documentation/pdfkit/pdf-widgets)
- Existing `pdf-goat --help` and namespace help in this repository
