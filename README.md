# pdf-goat

Local PDF tooling for macOS and Linux. The Rust CLI covers inspection, page edits,
conversion, extraction, security, and repair. Its workspace owns PDF parsing,
writing, text extraction, and rendering; no PDF engine library runs behind it.
The separate native macOS app remains a read-only PDFKit viewer.

No account and no document upload. Building dependencies and `setup meaning`
use the network. HTML inputs can fetch linked stylesheets, fonts, and images.
Document processing and searching stay local.

## Install

Requires [Rust 1.98 or later](https://rustup.rs/).

```bash
git clone https://github.com/aktanazat/pdf-goat.git ~/Documents/projects/pdf-goat
cargo build --release --manifest-path ~/Documents/projects/pdf-goat/Cargo.toml -p pdf-goat
mkdir -p ~/.local/bin
ln -sf ~/Documents/projects/pdf-goat/pdf-goat ~/.local/bin/pdf-goat
pdf-goat --help
```

The launcher resolves its own location with `readlink -f` (macOS 12.3 or later,
any Linux), so the clone can live anywhere. It runs the compiled release binary;
it does not install packages or build code on first use. Rebuild after pulling
source changes.

OCR uses Apple's Vision framework and requires macOS. Speech conversion uses
the macOS speech tools. `convert from-office` needs `office2pdf` on `PATH`.
The `office` family uses LibreOffice for macOS, installed with
`brew install --cask libreoffice` in `/Applications/LibreOffice.app`.
Ordinary PDF commands need neither Python, Ghostscript, qpdf, nor Tesseract.

### Verify a source build

The complete test suite needs the pinned meaning model, the macOS Helvetica
font collection, and a system font covering Japanese. Missing or invalid assets
fail their checks rather than silently skipping them. The tests never download
the model. Use `PDF_GOAT_HOME` to keep model and command state in a separate
directory.

```bash
cargo run -p pdf-goat -- setup meaning
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

## Use

```bash
pdf-goat info report.pdf
pdf-goat merge a.pdf b.pdf -o out.pdf
pdf-goat extract report.pdf --pages 2-5,9 -o excerpt.pdf
pdf-goat redact statement.pdf --find "[0-9]{3}-[0-9]{2}-[0-9]{4}" -o clean.pdf
pdf-goat security sign contract.pdf -o signed.pdf
pdf-goat render report.pdf --pages 1 --dpi 150 -o renders
pdf-goat edit add-text form.pdf --text 'Jane Q. Member' --font 'Brush Script MT' --size 20 --at 90,612 -o signed.pdf
```

`pdf-goat --help` lists the command families, and `pdf-goat <family> --help`
lists their verbs. Every run is appended to a SQLite ledger at
`~/.pdf-goat/ledger.db`; read it with `pdf-goat jobs`.

To sign a flat form, one without fillable fields, find each label with
`search` and draw beside its rectangle with `edit add-text`: a typed signature
in a script font such as `--font 'Brush Script MT'` or `--font 'Snell
Roundhand'`, a date in the default Helvetica, and `--text ✔ --font
ZapfDingbats` for a printed check box. `--at` is the start of the text's
baseline, in the coordinates `search` returns; `--fit --rect x0,y0,x1,y1`
instead draws at the largest size that fits the box. TrueType, OpenType, and
Type 1 fonts are embedded as subsets and kerned by their own tables, and
`--face` picks one face of a `.ttc` collection. `\n` in `--text` breaks a line,
`--width` wraps inside a width, and `--align`, `--rotate`, `--opacity`, and a
gray, RGB, or CMYK `--color` shape the result. `edit add-image` places a PNG or
JPEG signature inside `--rect`, keeping its aspect ratio unless `--stretch`, and
also takes `--rotate` and `--opacity`. `--pages 1,3` draws the same thing on
each listed page. Each run writes a new file; check it with
`render --mark x0,y0,x1,y1` on the returned `bbox`, which outlines that
rectangle on the rendered page.

Page-by-page verbs such as `text`, `search`, `count`, and `render` start
sequentially. After 200 ms, they use worker threads if at least eight pages
remain. The worker ceiling is eight by default, or `PDF_GOAT_WORKERS`, capped
at the CPU count. Each worker opens its own document handle.
`PDF_GOAT_WORKERS=1` keeps page work sequential.

The CLI stores a derived text cache at `PDF_GOAT_HOME/cache.sqlite`. The
`PDF_GOAT_CACHE_MB` setting limits cached row payloads to 256 MiB by default.
SQLite bookkeeping can make the file larger, and any budget under one byte,
including zero and negative numbers, turns the cache off. A value the CLI
cannot turn into a byte count, `inf` among them, uses the default.
`--no-cache` bypasses the cache, and the file can be deleted at any time.

Cache identity uses the file size, nanosecond modification time, and a
digest of the first and last MiB. Use `--no-cache` after a tool preserves
all three while changing bytes in between. A warm answer also trusts the
page count the cache recorded. A count the stored pages contradict, or one
too large for the file to hold, is dropped and the document is read again; a
damaged count that stays consistent with the pages stored beside it cannot
be detected from the store alone, so `text`, `count`, and `search` answer
from the cached pages whenever every page they select is already stored. A
verb that has to read a page the cache lacks sees the real count, drops the
row, and answers over the whole file. A confined answer is short rather than
wrong, and a `search --pages` request above the damaged count fails with an
explicit range error instead of a partial result. `--no-cache` reads the
file instead, and deleting the cache file has the same effect. A cache file
another process holds locked costs a verb about two seconds before
extraction runs live; a damaged or unreadable file costs about as much as an
uncached run.

For agents, the CLI writes JSON when its output is piped, and `--agent` forces
JSON on a TTY. Start with `pdf-goat --agent capabilities` for the family list,
then ask one family for its argument schema. Coding agents working in this
repository start from [AGENTS.md](AGENTS.md); the
[PDF skill](.agents/skills/pdf/SKILL.md) gives tested commands for each
workflow and the fields to check after each step.

```bash
pdf-goat --agent capabilities pages
pdf-goat --agent search report.pdf invoice --first
pdf-goat --agent transcript read transcript.pdf --conferred 2026-06-12
```

## MCP server

`pdf-goat-mcp` serves the same commands to MCP clients over stdio. It has no
PDF code of its own: each tool call runs the `pdf-goat` release binary built
beside it, and the build in [Install](#install) produces both. Register the
launcher in the clone with the client, by absolute path:

```json
{
  "mcpServers": {
    "pdf-goat": { "command": "/path/to/pdf-goat/pdf-goat-mcp" }
  }
}
```

Three tools cover every command. `capabilities` looks up commands and their
arguments, `run` runs one command and returns its JSON, and `render` returns a
page as an image, optionally clipped, with rectangles outlined to check
positions. A large result comes back shortened, with the whole JSON saved to a
file. [`docs/AGENT_PROTOCOL.md`](docs/AGENT_PROTOCOL.md#mcp-mapping) describes
the mapping.

## Agent-driven Office editing

Agents can create, inspect, and edit Writer documents, Calc spreadsheets, and
Impress slides without opening a window. A trusted Python script gets the
LibreOffice document model as `document`, plus `desktop`, `uno`, and
`prop(name, value)` for UNO properties. The script runs inside LibreOffice's
own Python runtime. The Rust CLI does not supply a Python environment.

For example, save this as `edit.py` to replace text in a Writer document:

```python
replacement = document.createReplaceDescriptor()
replacement.SearchString = "Draft"
replacement.ReplaceString = "Reviewed"
print(document.replaceAll(replacement))
```

```bash
pdf-goat --agent capabilities office
pdf-goat --agent office run edit.py --input report.docx -o reviewed.docx
pdf-goat --agent office export reviewed.docx -o reviewed.pdf
pdf-goat --agent office run create.py --new calc -o budget.xlsx
```

Use `--new writer`, `--new calc`, or `--new impress` to create a document.
Omit `-o` when a script only inspects a file; its printed output is returned
in the JSON `stdout` field. To recalculate formulas after editing Calc cells,
the script can call `document.calculateAll()`.

Writer saves DOCX, ODT, or PDF; Calc saves XLSX, ODS, or PDF; Impress saves
PPTX, ODP, or PDF. Each job uses a private input copy and a temporary office
profile. The command refuses an output that aliases its input or script.
The destination is replaced only after the job succeeds. A timeout or command
cancellation stops the job's office processes. `--timeout` defaults to 120 seconds.

Only run scripts you trust: this is ordinary local Python, not a sandbox.
Embedded document macros and automatic link updates are disabled on load;
the supplied script still has your file and network access. Office format
round-trips can change layout or unsupported features. Inspect the exported
PDF before replacing an original. The native PDF viewer does not edit Office files.

## Output and verification

- Generated PDF bytes and compression sizes can differ from older releases.
  Installed fonts affect PDFs that omit font programs. HTML conversion rasterizes
  SVG artwork at twice CSS resolution and adds a positioned, searchable text
  layer for its visible labels.
- Shadings (axial, radial, function-based, triangle meshes, Coons and tensor
  patches) and tiling patterns are rasterized the way MuPDF does: the same
  256-entry color table, triangle tessellation and fixed-point fill, MuPDF's
  Background, BBox and clipping rules, and tiling cells copied at MuPDF's pixel
  offsets. On 54 synthetic shading pages at 72, 144 and 300 dpi, 153 of 162
  renders are within one level per channel of PyMuPDF and 135 are identical.
  Remaining differences: DeviceCMYK colors can differ by up to 8 levels, in
  plain fills as well, because PyMuPDF converts them through an ICC profile; a
  45 degree axial gradient can differ by up to 3 levels on a few hundred pixels;
  antialiased edges of strokes and non-rectangular clips are not pixel-matched.
- `pages flatten` bakes annotations and form fields into page content. Page
  content, including transparency, stays vector and unchanged, so it keeps full
  detail at any zoom.
- OCR uses macOS Vision. On 21 synthetic scans (100 to 600 dpi, small text,
  accents, digits, tables, two columns, rotated, skewed, noisy and bilevel pages)
  it reads as accurately as ocrmypdf with Tesseract on all but two pages: Vision
  reads a yen sign as a dot and skips a lone digit in a table cell. A page that is
  one scanned image is read at the scan's own resolution (150 to 600 dpi). The
  hidden text is fitted to each word's ink, so search highlights and selection
  sit on the scanned words. The output is PDF/A-2b and passes veraPDF. When the
  source holds something PDF/A cannot carry, such as a font that cannot be
  embedded, the plain OCR file is written instead, `standard` is null and
  `warnings` says why.
- `convert pdfa` prepares PDF/A-2b output but does not run an external validator.
  Its `conformance_validated: false` result is deliberate. Validate archival
  deliverables with an independent tool such as veraPDF.
- `security sign` signs with your own certificate from a .p12/.pfx file (`--p12`, with
  the password in the environment variable `--password-env` names) as a PAdES B-B
  signature, or B-T with a time-stamp from `--tsa <url>`. `--rect` (in the `search`
  frame) and `--page` make it visible, showing `--appearance-text` and/or
  `--appearance-image`. Without `--p12` it makes a self-signed demonstration signature.
  `--field <name>` signs an existing empty signature field in place, inside its box when
  it has one; a name the form does not have makes a new field. `--certify 1|2|3` makes a
  certification signature that allows, after signing, no changes, form filling and
  signing, or also annotations; it must be the document's first signature.
  `--ltv` (with `--p12`) also stores the certificate chain and each certificate's OCSP
  or CRL answer in the document, so the signature can be checked offline later; with
  `--tsa` it then adds a document time-stamp over them. That makes PAdES B-LTA only
  when every certificate below the root has an answer or, by RFC 9608, needs none; a
  certificate that names no OCSP responder or CRL is listed in `dss.unchecked` and
  leaves the signature at B-T. Without `--tsa` the level stays B-B.
  In `security verify`, `trusted` checks the local signature policy. `chain_trusted`
  checks the certificate chain against the system's roots and any `--trust` files,
  holds it to the name constraints and certificate policies its authorities set
  (RFC 5280), and fails on any other critical extension it does not process;
  `revocation` reports the answers the document stores, or those `--online` fetches,
  and is `unknown` for a certificate nothing answers for.
  `timestamp`, `timestamp_valid`, and `pades_level` report the signature time-stamp and
  the PAdES level reached, up to B-LTA.
  `certified`, `docmdp_level`, and `changes_allowed` report whether the signature
  certifies the document, at which level, and whether every later change stays within
  what it allows, field locks included.

## Native macOS app

```sh
./tools/build-app
open ".build/PDF Goat.app" --args /path/to/document.pdf
```

The app opens local PDF files without editing them. Cmd-F opens word search
below the toolbar. Cmd-Shift-F selects meaning search. Press Return to search,
Cmd-G or Cmd-Shift-G to move between results, and Escape to close the bar.
Search uses the CLI installed at `~/.local/bin/pdf-goat`.

Meaning search needs a one-time setup:

```sh
pdf-goat setup meaning
pdf-goat search report.pdf "spending plan" --meaning --limit 5
```

The pinned local model ranks extracted passages, including ones that do not
contain the query words. A high rank is not proof of a match. The app highlights
each result in the original PDF so you can read it in context. Scanned pages
need OCR before they can be searched.

[System design](docs/SYSTEM.md), [agent protocol](docs/AGENT_PROTOCOL.md),
[feature ledger](docs/FEATURES.md), and [implementation plan](docs/PLAN.md)
define the target system.

## Benchmark: opening real files against Preview

Median milliseconds from the open request to confirmed page content on screen.
The fresh lane launches a new process for every trial. The warm lane reuses a
running app after one unmeasured prime.

| Document | Pages | Size | Fresh: pdf-goat | Fresh: Preview | Warm: pdf-goat | Warm: Preview |
| --- | --- | --- | --- | --- | --- | --- |
| pst-geo | 51 | 137.6 MB | **532** | 809 | **345** | 439 |
| ferc | 1063 | 4.9 MB | **479** | 733 | **301** | 486 |
| dive | 1151 | 44.7 MB | **453** | 748 | **281** | 511 |
| munzner | 422 | 72.9 MB | **821** | 1281 | **602** | 1009 |

pst-geo is vector map artwork at 2.7 MB per page. ferc is a long text order.
dive and munzner are illustrated textbooks.

Main-process physical footprint 750 ms after content appeared ran 251 to
449 MiB for pdf-goat and 267 to 488 MiB for Preview. Preview used less on one
group, the warm munzner lane, at 346 MiB against 429 MiB.

Apple M4 Pro, 24 GiB, macOS 26.6.2 build 25G83, AC power, disk caches not
purged, 2026-09-02. Fresh lane n=3 per app and document, warm lane n=2. All 48
trials were valid and 40 were measured. Per-trial values, deviations, p95, the
window and readiness rules, and the harness digest are in
[`benchmarks/results/viewer-comparison-summary.json`](benchmarks/results/viewer-comparison-summary.json)
and
[`benchmarks/results/viewer-comparison-runs.jsonl`](benchmarks/results/viewer-comparison-runs.jsonl).
Readiness means visible page content, not a fully painted page.

### Reproduce

`run` needs Accessibility and Screen Recording permission and writes raw
receipts only to the path you pass.

```sh
OUT=/absolute/path/to/output
swift benchmarks/pdf_benchmark.swift generate --output "$OUT/corpus"
swift benchmarks/pdf_benchmark.swift self-test
swift benchmarks/pdf_benchmark.swift run \
  --corpus "$OUT/corpus" --output "$OUT/session.jsonl" \
  --pdf-goat ".build/PDF Goat.app" \
  --preview "/System/Applications/Preview.app"
swift benchmarks/pdf_benchmark.swift summarize "$OUT/session.jsonl" \
  --output "$OUT/summary.json"
```

`generate` writes two synthetic fixtures. To time your own files instead, add
entries with a `path`, a `sha256`, and a `byte_count` to the corpus manifest;
`run` verifies each file against its digest before copying it into the session.
[`viewer-comparison-corpus.json`](benchmarks/results/viewer-comparison-corpus.json)
is the manifest behind the table above. Comparators are optional and each needs
an explicit app path: `--preview`, `--skim`, `--pdfgear`.

## License

AGPL-3.0.
See [LICENSE](LICENSE).
