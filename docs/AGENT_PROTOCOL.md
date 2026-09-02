# Agent protocol

Status: the standalone Python commands described as current are implemented.
The nine-family live protocol, native CLI, MCP mapping, durable findings, and
live job model are target contracts for later milestones.

## Goal

An agent must be able to understand and change a PDF without driving pixels, guessing coordinates, rereading the whole file, or trusting a mutation merely because the command returned success.

The protocol is transport-independent. `PDFGoat.app`, the native `pdf-goat` CLI, and the MCP adapter use the same request and response types.

## Core terms

| Term | Meaning |
| --- | --- |
| Document ID | Durable workspace identity for one source PDF |
| Revision | Monotonic version of the current working copy |
| Page ID | Durable page identity that survives reorder |
| Object ID | Identity for an annotation, form widget, link, image, or other addressable object |
| Result set | Revision-bound collection returned by search or observation |
| Selection | Durable identity for one named page, object, text, or region set |
| Finding ID | Durable workspace identity for one evidence-linked finding |
| Job ID | Identity for long-running work |
| Operation ID | Identity for one attempted state-changing operation |
| Receipt | Structured record of an operation and its verified effect |

A page index is only a current display position. Commands mutate page IDs or a named selection.

## Control surface

The native `pdf-goat` CLI will have nine command families:

1. `capabilities` discloses available operations and their schemas.
2. `session` opens and closes app sessions and documents, lists and inspects them, and returns the document ID and open response.
3. `observe` reads document, page, object, selection, result-set, finding, and operation structure.
4. `search` finds text, regular expressions, annotations, fields, links, or saved findings.
5. `render` writes a page or region image with optional object overlays.
6. `change` validates or applies one transaction.
7. `job` starts, reports progress on, cancels, and resumes long work.
8. `verify` checks a completed operation through an independent read path.
9. `remember` saves a provenance-linked finding, bookmark, or named selection.

The current file-in file-out verbs remain standalone, keep their names and JSON
contracts, and do not require a running app. `capabilities` also works
standalone. In the target protocol, `session open` starts or contacts the app.
The remaining family commands target its returned session and document IDs. A
request cannot mix standalone file paths with live IDs.

The standalone and live command vocabularies remain distinct. The live protocol
uses canonical operation names such as `pages.reorder`,
`security.apply_redactions`, and `edit.replace_text`. A capability record gives
the equivalent standalone invocation when one exists.

Milestone 2 moves the current Python command implementation behind the private
`pdf-goat-worker` name. The public `pdf-goat` command remains one native
executable.

## Progressive disclosure

The current standalone command uses `pdf-goat --agent capabilities` to return root arguments, top-level family names, standalone command names, a command count, the requested selector, and schemas. Schemas stay empty until a selector is supplied. `pdf-goat --agent capabilities pages` returns the `pages` family schema. The nine-family live protocol below is a later milestone.

The MCP adapter follows the same pattern. It exposes the nine family tools and serves detailed schemas as resources. It does not register one tool for every annotation shape, conversion target, or page operation.

A capability record reports:

- Whether the capability is native or uses the out-of-process worker.
- Whether it is available in standalone mode, live-session mode, or both.
- Whether it is synchronous or job-based.
- Whether it changes the working copy.
- Whether it can invalidate a digital signature.
- Whether it requires a dry run.
- Its input, output, and error schema versions.

## Standalone verb results

Every standalone verb returns one JSON object per run. Five of them bound or reshape that object.

`search` returns every hit on every page by default. `--first` stops at the first hit, `--limit N` stops after N hits, and `--pages RANGE` restricts the scan to a page range. `--first` is `--limit 1`; when both are given the last one wins. The result carries `truncated`, which is true only when a limit stopped the scan before the selected pages ran out. A caller that needs the full count runs the same query without a limit.

`preflight` reports a page in its `empty_pages` finding when the page declares no font and no image. It does not extract page text, so a page that declares a font but draws no glyphs is not reported empty, and neither is a blank page whose font resources are inherited from the page tree.

`text` with `-o` and without `--layout` streams the page text to that file. The result then reports `page_count` and the file path in `outputs`, and carries no `pages` list. Without `-o` the result carries the per-page text in `pages`. `--layout` always returns the per-page layout in `pages`, with or without `-o`.

`get images` writes each stored stream as it stands: `.jpg` for DCT, `.jp2` for JPX, `.tif` for CCITT, `.tiff` for CMYK rasters, `.png` otherwise. A stencil mask is written with the PDF's sample values, not its painted appearance. An image pikepdf cannot read falls back to MuPDF, which re-encodes it.

`split`, `extract`, `compress`, and `convert from-office` write through a sibling `.part` file and rename it over the destination. A failed run leaves neither file. An existing read-only destination is replaced, because the rename needs write access to the directory, not the file.

## Request envelope

Every live document request has this outer shape:

```json
{
  "protocol_version": 1,
  "request_id": "req_01K4D6EJ8S8E9WQ2J0R7C2Y6WA",
  "command": "change",
  "session_id": "session_01K4D6DZT0KR2N1F4J4BA8KX5P",
  "document_id": "doc_01K4D6E1X24TGXWQ2MY7H3BX5E",
  "if_revision": 12,
  "arguments": {}
}
```

`capabilities` omits session and document identity. `session open` omits those fields and supplies a source path plus the requested access grant in `arguments`; its response issues both IDs. Standalone verbs use their existing path arguments and the same structured result types.

`protocol_version` covers the envelope, errors, and receipts. Each family carries its own `schema_version`; a family schema bump does not change `protocol_version`.

Read-only commands may omit `if_revision` when the caller accepts the latest revision. A command that reuses a result set, selection, region, or object always supplies the revision that created it.

### Request identity and retries

For each `(session_id, request_id)`, the target protocol persists one canonical
request digest before it executes a side effect. Repeating the same ID and
digest returns the original response without another execution. Reusing the ID
with a different digest returns `invalid_request`.

After an unknown transport outcome, the caller uses `observe operation` with
the `request_id` to retrieve the attempt. It does not submit the side effect
again with a new ID.

## Open response

`session open` returns enough information to choose the next observation without reading every page:

- Document ID and revision.
- Source display name, working-copy state, `source_state`, and source and working-copy fingerprints. `source_state` is `matches_fingerprint`, `changed_externally`, or `source_missing`.
- Page count and page-size summary.
- Encryption and permissions.
- Digital-signature state.
- Outline depth and top-level entries.
- Counts for annotations, forms, attachments, and bookmarks when available without a full scan.
- Embedded-text coverage estimate.
- OCR and index state.
- Active warnings and degraded-mode state.

Unknown values are `null` with a reason. The system does not turn an expensive full scan into an implicit open step.

### Source-state transitions

Allowed transitions are:

```text
matches_fingerprint -> changed_externally
matches_fingerprint -> source_missing
changed_externally -> matches_fingerprint   only after explicit reload or source replacement
source_missing -> matches_fingerprint       only after explicit relocation or source replacement
```

No mutation or standalone read silently clears `changed_externally` or
`source_missing`.

## Observation

`observe` accepts a scope, fields, and response budget. Common scopes are:

- `document`
- `pages`
- `page`
- `objects`
- `selection`
- `result_set`
- `findings`
- `operation`

Budget fields include `max_pages`, `max_items`, `max_characters`, and `cursor`. Omitted budgets default to 8 pages, 200 items, and 20,000 characters. A truncated response always returns `next_cursor` and the exact omitted count when known.

Text blocks report:

- Page ID and current page index.
- Text.
- PDF-point bounds and page box.
- Reading-order position and confidence.
- Source: embedded text or OCR.
- Source revision.

The agent requests a render only when structure is insufficient. Typical reasons are handwriting, diagrams, overlapping objects, missing reading order, visual comparison, or redaction review.

## Search and reusable result sets

Search accepts `max_matches`, `max_characters`, and `cursor`. It returns a result-set ID and compact matches, with at most 200 matches and 20,000 characters by default. Each match has a result ID, page ID, text excerpt, bounds, source, and confidence where applicable. A truncated result returns `next_cursor` and the total match count when known.

A later command can target:

- Explicit page or object IDs.
- Result IDs from one result set.
- A named selection.
- A search predicate plus an expected match count.

The expected count is a mutation precondition. A redaction command that expected 18 account-number matches fails if the current revision has 17 or 19.

Result sets are revision-bound. A document change does not silently retarget them.

## Rendering

`render` writes PNG or JPEG output to the workspace and returns:

- File path.
- Media type.
- Pixel dimensions.
- SHA-256 digest.
- Page ID, page box, rotation, PDF-point region, and render scale.
- Overlay legend when object IDs are drawn.

The protocol never returns base64 image bytes by default. A caller can render one page, a page range, or one rectangle. Full-document rendering requires an explicit page range and becomes a job.

## Changes and transactions

A `change` request contains one or more operations that commit atomically. Operations use typed names such as:

- `pages.rotate`
- `pages.reorder`
- `annotate.highlight`
- `form.set_value`
- `security.apply_redactions`
- `edit.replace_text`
- `document.undo`
- `document.redo`
- `document.export_copy`

Example:

```json
{
  "protocol_version": 1,
  "request_id": "req_01K4D71DVX9CQQQEV6GM2R4XQG",
  "command": "change",
  "session_id": "session_01K4D6DZT0KR2N1F4J4BA8KX5P",
  "document_id": "doc_01K4D6E1X24TGXWQ2MY7H3BX5E",
  "if_revision": 12,
  "arguments": {
    "dry_run": false,
    "operations": [
      {
        "name": "pages.rotate",
        "page_ids": ["page_01K4D6F9H12Z3M8VSKRY1V4Q8T"],
        "degrees": 90
      }
    ]
  }
}
```

The engine validates all operations before applying the first one.

### Mutation attempt states

Allowed transitions are:

```text
received -> rejected
received -> executing -> failed
received -> executing -> applied
```

`rejected`, `failed`, and `applied` are terminal. `rejected` means validation,
permission, dry-run, or revision checks failed before execution. `failed`
means execution or independent verification failed. Both leave the live
revision unchanged and create no final output path. A safe temporary artifact
may remain in diagnostic data.

The engine applies operations and runs independent checks against candidate
bytes. It commits the working copy, revision, effect, and receipt before it
returns `applied`.

Undo and redo are transactions. They take the top `undo_id` or `redo_id` from the current session stack, increment the revision, and return an inverse receipt. An older or expired ID fails with `precondition_failed`; undo IDs are not durable history capabilities.

`document.export_copy` writes a new file, leaves the working-copy revision unchanged, and returns a receipt with an `outputs` entry. It requires the `export_copy` grant.

A dry run is mandatory for:

- Permanent redaction.
- Sanitization.
- Flattening.
- Certificate signing.
- Editing a signed document.
- Replacing the full document with transformer output.
- Any operation that reports an uncertain target or lossy conversion.

A dry run returns exact targets, signature effects, permissions, expected output, validation steps, warnings, and a `dry_run_digest` over the current revision, canonical operations, and resolved target IDs. Applying a mandatory-dry-run operation must repeat the same operations at the same revision and supply that digest. No mutable plan object is stored.

## Receipts

Every attempted state-changing operation returns and persists a receipt. This
is an abbreviated successful receipt:

```json
{
  "operation_id": "op_01K4D72T59JHF6FPEK3TD76V1E",
  "status": "applied",
  "document_id": "doc_01K4D6E1X24TGXWQ2MY7H3BX5E",
  "revision_before": 12,
  "revision_after": 13,
  "changed": [
    {
      "page_id": "page_01K4D6F9H12Z3M8VSKRY1V4Q8T",
      "kind": "page_rotation",
      "before": 0,
      "after": 90
    }
  ],
  "outputs": [],
  "identity_map": null,
  "undo_id": "undo_01K4D72T5J5Y4DFFDVBF5H0VZR",
  "redo_id": null,
  "verification": {
    "status": "passed",
    "checks": ["page_rotation", "document_reopen"]
  },
  "warnings": []
}
```

A complete receipt also includes:

- `request_id`: the request used for idempotency.
- `request_digest`: the digest of the canonical request body.
- `protocol_version` and the family `schema_version`.
- `status`: `rejected`, `failed`, or `applied`.
- `operations`: canonical operation names without secret argument values.
- `error`: `null` for `applied`; otherwise `{code, message, recovery}`.
- `verification.results`: one object per check with `name`, `read_path`,
  `status`, the checked revision or output digest, and `artifact_path` when
  applicable.

For `rejected` and `failed`, `revision_after == revision_before`, `changed == []`,
and `outputs == []` for final outputs. Both statuses set `undo_id == null` and
`redo_id == null`.

An export or split keeps one `outputs` entry per file with path, media type,
SHA-256 digest, and page IDs when applicable. If a requested final path exists,
the operation returns `precondition_failed` with the current digest and does
not overwrite it. A whole-document replacement adds an `identity_map` with
unambiguous before and after page and object IDs plus unresolved entries.
Unresolved entries mark their findings and selections stale.

A process exit code is not verification. The workflow handler defines observable postconditions, and the engine checks them before returning `applied`.

## Verification

`verify` takes an `operation_id` and reruns that operation's required checks against persisted document bytes or output artifacts. It returns the operation ID, checked revision or output digest, overall status, and one result per check with the check name, independent read path, status, and artifact path when applicable.

Capability schemas own the required check list. Read paths include extraction, object inspection, raster comparison, OCR, signature validation, and document reopen. Redaction requires extraction, object inspection, raster comparison, re-OCR of every redacted page, and document reopen. The receipt records checks performed at commit time; `verify` is a repeatable later observation, not a copy of that receipt.

## Long-running jobs

`job start` validates and queues OCR, full comparison, conversion, optimization, repair, indexing, and large exports, then returns a job ID. The request names the operation, input revision, and family schema version.

A job record reports:

- State: `queued`, `running`, `cancelling`, `paused`, `completed`, `failed`, or `cancelled`.
- Current unit and total units when known.
- Last completed page or object.
- Current phase.
- Whether cancellation is available at the current boundary.
- Input revision and source fingerprint.
- Capability and schema version.
- Last durable checkpoint and its version.
- Temporary output path when produced.
- Candidate-output validation and the later `import_operation_id` when imported.

Cancellation is cooperative at page, tile, or transformer-process boundaries. A resumable cancellation ends in `paused`; work without a safe checkpoint ends in `cancelled`. The protocol does not claim immediate cancellation inside an Apple framework call that has no cancellation API.

Allowed job transitions are:

```text
queued -> running | cancelled | failed
running -> cancelling | completed | failed
cancelling -> paused | cancelled | failed
paused -> queued
```

`completed`, `failed`, and `cancelled` are terminal. A successful `job resume`
moves `paused` to `queued`. For resumable work, `job cancel` moves
`running -> cancelling -> paused`. For work without a safe checkpoint, it moves
`running -> cancelling -> cancelled`.

`completed` means that the job produced and validated a candidate output. It
does not mean that the live document changed. Import remains a separate
`change` with the job input revision. The job record stores the later
`import_operation_id`; it does not claim a final operation receipt before
import.

`job resume` names a paused job ID. It returns `stale_revision` when the working
copy changed, `source_changed` when the source fingerprint changed, or
`precondition_failed` when the capability schema or checkpoint format no longer
matches. A failed resume leaves the job `paused` and preserves its checkpoint
and temporary output. Importing completed job output is a `change` whose
`if_revision` equals the job input revision. A mismatch preserves the temporary
output and leaves the live document unchanged.

## Errors

Errors have stable machine codes and human-readable recovery data.

| Code | Meaning | Recovery data |
| --- | --- | --- |
| `invalid_request` | The request mixes profiles or omits required identity | Accepted standalone or live request shape |
| `session_unavailable` | The selected app session or local IPC route is unavailable | App and consent state plus safe retry action |
| `source_changed` | Source bytes differ or the source is active in another owner | Before and after fingerprints, live session when present, and reload or keep-working-copy choices |
| `precondition_failed` | A declared count, stack position, permission, or other mutation condition differs | Expected value, actual value, and the smallest resolving observation |
| `dry_run_required` | A required digest is absent or does not match this revision and operation | Required dry-run arguments and mismatch reason |
| `stale_revision` | The working copy changed | Current revision and changed object summary |
| `stale_reference` | A result, region, or object no longer names current content | Current revision and replacement observation hint |
| `needs_password` | The document is locked | Encryption type and allowed next action |
| `permission_denied` | PDF or system permission blocks the operation | Permission and affected operation |
| `signature_would_break` | The operation changes signed bytes | Signature identities and save-copy option |
| `unsupported_content` | The selected engine cannot perform the operation correctly | Missing capability and available non-mutating observations |
| `validation_failed` | Proposed output failed a workflow postcondition | Failed checks and preserved temporary output when safe |
| `worker_failed` | The out-of-process worker failed | Exit status, bounded diagnostic path, and unchanged revision |
| `cancelled` | The caller cancelled at a supported boundary | Last completed unit and unchanged revision |
| `degraded_document` | The file crossed a measured performance or parser boundary | Active degraded mode and available operations |

`invalid_request` also covers reuse of one `request_id` with a different
request digest. `precondition_failed` also covers an existing output path and
a job schema or checkpoint mismatch.

For an unsupported operation, the app returns `unsupported_content` and does
not change the document.

## Access and safety

Agent access has three session grants:

- `inspect`
- `edit_working_copy`
- `export_copy`

The user grants a level for the session. Reversible edits do not prompt again. Permanent overwrite of the source remains a human Save or Replace action in the native app.

The `inspect` grant permits an explicit `remember` action because it changes workspace records, not PDF bytes. `edit_working_copy` and `export_copy` control their named effects.

A transformer job receives no account credential, private key, or unrelated
file access. An encrypt or decrypt job may receive one operation-scoped PDF
password through an inherited anonymous pipe or private file descriptor. A
password never enters command arguments, environment variables, receipts,
logs, saved commands, durable state, or MCP responses. The native worker
interface removes the current password-on-argv options.

## Saved workspace records

`remember` saves only one of these records:

- Bookmark.
- Named selection.
- Finding.

A named selection has a durable selection identity. A finding has a finding ID
and requires document ID, source revision, page or object evidence, quoted text
or region, author, and confidence. `remember` returns the record ID, source
revision, and a receipt.

A finding has one allowed state transition:

```text
current -> stale
```

A revision that touches cited evidence makes the finding stale. Revalidation
creates a new finding ID and evidence link. The old finding remains stale and
unchanged.

Search indexes, OCR output, thumbnails, and temporary result sets are caches. They are not findings.

Worker-produced text first returns as a file-backed artifact with a digest and source provenance. It becomes an observation or evidence source only after `DocumentEngine` imports and validates it against the named revision.

`remember` accepts only the three record types above. The
[feature ledger](FEATURES.md#product-exclusions) owns excluded product scope.

## MCP mapping

The MCP adapter maps one-to-one to the nine command families. It adds no policy, state, retries, or PDF implementation.

Resources expose:

- Capability family schemas.
- Open document summaries.
- Paged observations.
- Job and receipt records.
- Rendered artifact metadata.

The adapter returns concise results and paths. An agent can call `pdf-goat`
directly without loading MCP schemas.

## Protocol acceptance tests

The protocol is ready when an agent can complete these without UI automation:

1. **Milestone 2:** Open a document, inspect its outline, find a phrase, render one ambiguous region, and cite the correct page and bounds.
2. **Milestone 2:** Reorder and rotate pages in one transaction, verify the result, undo it, and prove the original order returned.
3. **Milestone 5:** Mark and apply redactions to a copy, then prove the target text is absent from extraction, objects, re-OCR, and pixels.
4. **Milestone 4:** Start OCR on a scanned document, observe progress, cancel it at a resumable boundary, resume from the checkpoint, and keep the UI responsive.
5. **Milestone 2:** Detect an external edit and reject a stale mutation without changing either file.
6. **Milestone 4:** Save a finding with evidence, change the cited page, and observe that the finding becomes stale.

## Deterministic synthetic acceptance matrix

Use a fixed synthetic fixture `doc-A` at revision 12 with stable page IDs
`page-A` and `page-B`. Use fixed request IDs and output paths. These names are
test fixtures, not production examples.

| Case | Initial state and action | Required result |
| --- | --- | --- |
| Exact duplicate | Send `pages.rotate` twice with the same `request_id`, digest, and `if_revision: 12` | One revision increment, one effect, and the same receipt returned twice |
| Conflicting duplicate | Reuse the same `request_id` with different degrees | `invalid_request`; revision remains 13 |
| Lost response | Apply once, drop the response, then query by `request_id` | Original `applied` receipt returns; no mutation is replayed |
| Stale revision | Send a change with `if_revision: 11` | `stale_revision`; failure receipt has revision 12 before and after |
| Atomic batch | Send two operations where the second has an invalid target | Neither operation applies; no undo ID or final output |
| Verification failure | Inject a failed reopen check after candidate generation | Live revision remains 12; no final path; safe temporary artifact is reported |
| Dry-run mismatch | Dry-run at revision 12, advance to 13, then apply the old digest | `dry_run_required` or `stale_revision`; no effect |
| Duplicate undo | Send the same undo request twice with one `request_id` | One undo transaction and one revision increment |
| Output collision | Seed the export path with known bytes, then export to it | `precondition_failed`; seeded bytes and digest remain unchanged |
| External source edit | Change the source fingerprint before a standalone command | `source_changed`; no output file |
| Resumable cancellation | Cancel OCR after checkpoint page 8 | `running -> cancelling -> paused`; checkpoint remains page 8 |
| Stale resume | Edit the live document, then resume that paused OCR job | `stale_revision`; job stays paused and checkpoint remains page 8 |
| Nonresumable cancellation | Cancel inside work with no safe checkpoint | `running -> cancelling -> cancelled`; live revision unchanged |
| Late job output | Complete a worker job from revision 12 after the document reaches 13, then import | Import returns `stale_revision`; candidate output remains available |
| Crash recovery | Crash after effect durability but before response delivery, then relaunch | Lookup returns the persisted receipt; the revision number is not reused |
| Ambiguous replacement | Replace the whole document with two ambiguous page matches | `identity_map` has unresolved entries; affected selections and findings become stale |
| Duplicate remember | Submit one `remember` request twice with the same request ID | One record ID and one receipt |
| Finding staleness | Change the cited region of a current finding | Finding changes to stale and keeps old evidence |
| Finding revalidation | Revalidate a stale finding at a newer revision | New finding ID; old finding remains stale |
| Grant denial | Run `change` with only `inspect` | `permission_denied`; no revision or workspace mutation |
| Bounded observation | Observe 9 pages with defaults | 8 pages, `next_cursor`, and exact omitted count when known |
| Unknown open field | Make attachment count require a full scan | `null` plus a reason; open does not perform the scan |
