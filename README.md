# pdf-goat

Local PDF tooling for macOS and Linux. One CLI covers inspection, page edits,
conversion, extraction, security, and repair. A native macOS app opens the same
files in a read-only PDFKit viewer.

No account and no upload. The CLI has no networking code; only the first-run
dependency install reaches the network.

## Install

Requires [uv](https://docs.astral.sh/uv/).

```bash
brew install ghostscript qpdf tesseract   # PDF/A and reduce, repair and flatten, OCR
git clone https://github.com/aktanazat/pdf-goat.git ~/Documents/projects/pdf-goat
mkdir -p ~/.local/bin
ln -sf ~/Documents/projects/pdf-goat/pdf-goat ~/.local/bin/pdf-goat
pdf-goat --help
```

The launcher resolves its own location, so the clone can live anywhere. First
run installs the Python dependencies through uv. On Linux, use the apt or dnf
equivalents of the brew line. Three more tools cover four verbs: `from-html`
and `from-md` need `weasyprint`, `convert` to Office formats needs
`office2pdf-cli`, and `convert audio` needs the macOS `say` binary.

## Use

```bash
pdf-goat info report.pdf
pdf-goat merge a.pdf b.pdf -o out.pdf
pdf-goat extract report.pdf --pages 2-5,9 -o excerpt.pdf
pdf-goat redact statement.pdf --find "[0-9]{3}-[0-9]{2}-[0-9]{4}" -o clean.pdf
pdf-goat security sign contract.pdf -o signed.pdf
pdf-goat render report.pdf --pages 1 --dpi 150 -o renders
```

`pdf-goat --help` lists all 39 command families, and `pdf-goat <family> --help`
lists their verbs. Every run is appended to a SQLite ledger at
`~/.pdf-goat/ledger.db`; read it with `pdf-goat jobs`.

For agents, the CLI writes JSON when its output is piped, and `--agent` forces
JSON on a TTY. Start with `pdf-goat --agent capabilities` for the family list,
then ask one family for its argument schema.

```bash
pdf-goat --agent capabilities pages
pdf-goat --agent search report.pdf invoice --first
pdf-goat --agent transcript read transcript.pdf --conferred 2026-06-12
```

## Native macOS app

```sh
./tools/build-app
open ".build/PDF Goat.app" --args /path/to/document.pdf
```

The app opens local PDF files. It does not edit them yet.
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

MIT. See [LICENSE](LICENSE).
