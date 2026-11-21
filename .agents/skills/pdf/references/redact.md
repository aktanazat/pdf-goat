# Redact and prove the text is gone

`redact` removes matching text, clears image pixels under it, paints a black box and clears matching field values. `IN` the PDF, `W` an empty work dir.

## 1. Find every occurrence first

```bash
pdf-goat --agent search "$IN" "123-45-6789"
pdf-goat --agent text "$IN" --mask '\d{3}-\d{2}-\d{4}'
```

- `text --mask` is a dry run of a pattern: check it catches every variant (spaces, no dashes).

## 2. Redact

```bash
pdf-goat --agent redact "$IN" --find '\d{3}-\d{2}-\d{4}' -o "$W/redacted-1.pdf"
pdf-goat --agent redact "$W/redacted-1.pdf" --find 'Acme Corp' -o "$W/redacted.pdf"
```

- `--find` is a case-insensitive regex over the page's words. A space matches any run of whitespace, line breaks included: `Acme Corp` also finds the name split across two lines.
- Returns `pattern`, `redactions` (areas removed) and `field_redactions` (form values cleared). `redactions` must be at least the `search` count from step 1; `0` means the pattern missed.

## 3. Prove it

```bash
pdf-goat --agent search "$W/redacted.pdf" "123-45-6789"
pdf-goat --agent search "$W/redacted.pdf" "Acme Corp"
pdf-goat --agent text "$W/redacted.pdf"
pdf-goat --agent render "$W/redacted.pdf" --pages 1 --dpi 144 --clip 50,115,420,215 --mark 337.8,191.9,404.5,204.8 --mark 197.8,124.6,253.6,137.5 -o "$W/look"
```

- Both searches return `count: 0`, and `text` contains neither value: the text is gone, not covered.
- Open the render: a black box fills each magenta outline (the old rects) and the rest of the line is intact.

## 4. Clean what redaction does not touch

```bash
pdf-goat --agent security sanitize "$W/redacted.pdf" -o "$W/sanitized.pdf"
pdf-goat --agent meta strip "$W/sanitized.pdf" -o "$W/clean.pdf"
pdf-goat --agent meta get "$W/clean.pdf"
```

- `sanitize` keeps title and author; `meta strip` clears them. Values can also hide in bookmarks, links, attachments.

## 5. Scans

`redact` finds text only, so a scan needs `ocr.md` first. Set `SCANTEXT` to the OCR'd file:

```bash
pdf-goat --agent redact "$SCANTEXT" --find 'Revenue grew' -o "$W/scan-redacted.pdf"
pdf-goat --agent search "$W/scan-redacted.pdf" "Revenue grew"
pdf-goat --agent get images "$W/scan-redacted.pdf" -o "$W/scan-images"
```

- Open the extracted image: the words must be blank in it. An OCR misread escapes the pattern: compare `text` with a render.
