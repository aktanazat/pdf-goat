# PDF Goat agent instructions

This repository contains the Rust CLI and the native AppKit viewer. The files
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

The [PDF skill](.agents/skills/pdf/SKILL.md) holds tested commands for every
PDF workflow and the fields to check after each one. Read it, and the
reference file it names for the task, before PDF work.

1. Start with `pdf-goat --agent capabilities`. Request one family or top-level
   command, such as `pdf-goat --agent capabilities edit`, when you need its
   schema.
2. Run `pdf-goat --agent preflight FILE` before processing an untrusted PDF.
   Use `inspect FILE --limit N` for a bounded page inventory.
3. Prefer `get text-blocks`, `get links`, and `get attachments` over
   rendering. `search` returns rectangles in the frame that
   `edit add-text --at`, `edit add-image --rect`, and `render --mark` take.
4. Use `--agent` for every scripted call. Treat a nonzero exit status or
   `"ok": false` as failure.
5. Give each mutation a new output path. Read the output back with a different
   command than the one that wrote it, and confirm that the source did not
   change.
6. Look at each changed region before you report it:
   `render OUT --pages N --mark X0,Y0,X1,Y1` outlines a `search` rectangle or
   an edit's `bbox`. `--clip` crops the page as displayed, which differs from
   that frame on a rotated page; see
   [Rectangle coordinates](docs/AGENT_PROTOCOL.md#rectangle-coordinates).
7. Report signature results as `security verify` returns them, including
   `pades_level`, `chain_trusted`, and `revocation`.

`pdf-goat-mcp` serves the same commands to MCP clients as three tools:
`capabilities`, `run`, and `render`, which returns the page image and takes
`clip` and `marks`. [MCP mapping](docs/AGENT_PROTOCOL.md#mcp-mapping) owns
its behavior.

Copilot sessions started from the repository's Agents tab first run
[`copilot-setup-steps.yml`](.github/workflows/copilot-setup-steps.yml), which
installs `pdf-goat` and `pdf-goat-mcp` on PATH. Choose the
[`pdf` agent](.github/agents/pdf.agent.md) for PDF work: it adds the MCP
tools. Those sessions run on Linux, where `convert ocr`, `convert audio`, and
the `office` commands are unavailable.

Use PDFGoat.app for visual review. Do not automate the app when the CLI exposes
the operation.

## Completion

Run the smallest check that exercises the changed public path. Reopen changed
PDFs in PDFGoat.app and applicable external readers. Run and inspect native UI
changes. Keep the signpost trace and corpus file for performance changes.
