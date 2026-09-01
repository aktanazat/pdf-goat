# PDF Goat agent instructions

This repository contains the Python CLI and the native AppKit viewer. The files
under `docs/` define the system and agent protocol.

## Documentation map

- [`docs/SYSTEM.md`](docs/SYSTEM.md) owns architecture and invariants. Read it
  before changing document ownership, workers, storage, concurrency, or process
  design.
- [`docs/AGENT_PROTOCOL.md`](docs/AGENT_PROTOCOL.md) owns command and response
  semantics. Read it before changing the CLI, MCP adapter, JSON schemas,
  receipts, stable IDs, or agent control.
- [`docs/FEATURES.md`](docs/FEATURES.md) owns product scope. Read it before
  adding a feature or changing competitor parity.
- [`docs/PLAN.md`](docs/PLAN.md) owns milestone order, performance budgets, and
  exit gates. Read the active milestone before implementation or performance
  work.
- Read all four documents before changing redaction, encryption, signing,
  sanitization, repair, or another security-sensitive PDF path.

Change the owner document once. Other documents should link to it instead of
copying the rule.

## Use the current PDF tools

1. Start with `pdf-goat --agent capabilities`. Request one top-level command,
   such as `pdf-goat --agent capabilities get`, when you need its schema.
2. Run `pdf-goat --agent preflight FILE` before processing an untrusted PDF.
   Use `inspect FILE --limit N` for a bounded page inventory.
3. Prefer `get text-blocks`, `get links`, and `get attachments` over
   rendering. Use `render --clip X0,Y0,X1,Y1` only when structure is not
   sufficient.
4. Use `--agent` for every scripted call. Treat a nonzero exit status or
   `"ok": false` as failure.
5. Give each mutation a new output path. Reopen the output, verify the requested
   result, and confirm that the source did not change.

Use PDFGoat.app for visual review. Do not automate the app when the CLI exposes
the operation.

## Completion

Run the smallest check that exercises the changed public path. Reopen changed
PDFs in PDFGoat.app and applicable external readers. Run and inspect native UI
changes. Keep the signpost trace and corpus file for performance changes.
