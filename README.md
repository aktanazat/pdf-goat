# pdf-goat

`pdf-goat` is a local command-line tool for PDF editing, conversion, inspection,
extraction, security operations, and repair. Run `pdf-goat --help` for the
current command list.

The CLI stores bounded job metadata in SQLite. It writes JSON when piped or
when you use `--agent`. Run `pdf-goat --agent capabilities` to list command
families, then request one family for its argument schema.

For academic transcripts,
`pdf-goat --agent transcript read transcript.pdf --conferred YYYY-MM-DD`
returns identity, terms, courses, printed issue dates, freshness results, and
provenance. `pdf-goat transcript resolve --root DIR` examines only entries in
the named directory and does not recurse. It ranks every candidate by the
printed issue date.
The caller must enumerate Drive files.

## Native macOS app

`PDFGoat.app` currently opens local PDFs in a read-only PDFKit viewer for
macOS. The documents below define the target native system:

- [System design](docs/SYSTEM.md)
- [Agent protocol](docs/AGENT_PROTOCOL.md)
- [Feature ledger](docs/FEATURES.md)
- [Implementation plan](docs/PLAN.md)

The install steps below are for the current Python CLI on macOS and Linux. The
native app runs only on macOS.

### Build and run the native viewer

Build the app bundle, then open a PDF:

```sh
./tools/build-app
open ".build/PDF Goat.app" --args /path/to/document.pdf
```

## Install (macOS / Linux)

Requires [uv](https://docs.astral.sh/uv/). A few verbs need system tools.

```bash
# 1. system deps (macOS; on Linux, use apt/dnf equivalents)
brew install ghostscript qpdf tesseract          # compress, repair, OCR
cargo install office2pdf-cli                      # DOCX/XLSX/PPTX->PDF (optional, Rust toolchain)

# 2. clone anywhere (the launcher self-locates; no hardcoded paths)
git clone https://github.com/aktanazat/pdf-goat.git ~/Documents/projects/pdf-goat

# 3. put it on PATH (make sure ~/.local/bin is on your PATH)
mkdir -p ~/.local/bin
ln -sf ~/Documents/projects/pdf-goat/pdf-goat ~/.local/bin/pdf-goat

# 4. first run installs the Python deps via uv automatically
pdf-goat --help
```

## Usage

```bash
pdf-goat info report.pdf
pdf-goat merge a.pdf b.pdf -o out.pdf
pdf-goat convert docx report.pdf -o report.docx
pdf-goat security sign contract.pdf -o signed.pdf
pdf-goat redact statement.pdf --find "[0-9]{3}-[0-9]{2}-[0-9]{4}" -o clean.pdf
pdf-goat --agent capabilities pages    # discover one command family
pdf-goat --agent inspect report.pdf --limit 10
pdf-goat --agent preflight report.pdf
pdf-goat render report.pdf --pages 1 --clip 72,72,540,720 -o renders
pdf-goat --agent info report.pdf        # JSON output for scripts and agents
```

Run `pdf-goat --help` (or `pdf-goat <namespace> --help`) for the full verb list.

## Reproducible benchmark

The `run` command needs Accessibility and Screen Recording permission. By
default, it records five fresh-process runs, one unmeasured warm prime, and
three measured warm opens for each app and fixture. It does not purge disk
caches. Raw JSONL goes only to the external path passed with `--output`.
`summarize` derives `summary.json` from that file. The repository keeps
selected experiment receipts under [`benchmarks/results/`](benchmarks/results/).

```sh
BENCH_ROOT="/absolute/path/to/benchmark-output"

swift benchmarks/pdf_benchmark.swift generate \
  --output "$BENCH_ROOT/corpus"

swift benchmarks/pdf_benchmark.swift self-test

swift benchmarks/pdf_benchmark.swift run \
  --corpus "$BENCH_ROOT/corpus" \
  --output "$BENCH_ROOT/session.jsonl" \
  --pdf-goat "/absolute/path/to/PDF Goat.app" \
  --preview "/System/Applications/Preview.app"

swift benchmarks/pdf_benchmark.swift summarize \
  "$BENCH_ROOT/session.jsonl" \
  --output "$BENCH_ROOT/summary.json"
```

`--preview` is optional. Every included comparator needs an explicit app path.
The summary reports the median, median absolute deviation, p95, minimum, and
maximum.
