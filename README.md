# pdf-goat

Acrobat-class PDF toolbox for the terminal — 85+ verbs across `convert`, `annotate`,
`form`, `security`, `pages`, `get`, `optimize`, `accessibility`, `compare`, and more:
merge / split / extract / rotate, render to image, images↔PDF, **OCR**, **true redaction**,
**digital sign + verify**, fill / create forms, **convert PDF↔Word/Excel/PowerPoint**,
HTML/Markdown→PDF, watermark, compress / reduce-to-size, read-aloud, page numbering /
Bates / n-up / booklet, text + visual diff, and metadata.

SQLite job ledger, JSON output when piped or with `--agent`.

## Install (macOS / Linux)

Requires [uv](https://docs.astral.sh/uv/). A few verbs need system tools.

```bash
# 1. system deps (macOS — Linux: use apt/dnf equivalents)
brew install ghostscript qpdf tesseract          # compress, repair, OCR
brew install --cask libreoffice                   # PDF<->Office conversion (optional)

# 2. clone anywhere (the launcher self-locates — no hardcoded paths)
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
pdf-goat --agent info report.pdf        # JSON output for scripts/agents
```

Run `pdf-goat --help` (or `pdf-goat <namespace> --help`) for the full verb list.
