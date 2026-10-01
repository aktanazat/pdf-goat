# Redact and prove the text is gone

`redact` removes the matching text from the page content, clears the same area of any image under it, paints a black box, and clears matching form field values. Set `IN` to the PDF and `W` to an empty work directory, both absolute.

## 1. Find every occurrence first

```bash
pdf-goat --agent search "$IN" "123-45-6789"
pdf-goat --agent text "$IN" --mask '\d{3}-\d{2}-\d{4}'
```

- `search` gives the count and rects to expect: here one hit, page 1, `[337.8, 191.9, 404.5, 204.8]`.
- `text --mask RE` shows the text with every regex match replaced by `[REDACTED]`, a dry run of what a pattern catches. Check it catches every variant (spaces, no dashes) before redacting.

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

- `security sanitize` reports `removed` (`javascript`, `embedded_files`, `attached_files`, `xml_metadata`, `thumbnails`). It keeps the title and author.
- `meta strip` clears those: `meta get` then shows `metadata` with only `format`. A value can also hide in bookmarks, link targets or attachments: check `get bookmarks`, `get links`, `get attachments` when it matters.

## 5. Scans

`redact` finds text only, so a scan needs `ocr.md` first. Set `SCANTEXT` to the OCR'd file:

```bash
pdf-goat --agent redact "$SCANTEXT" --find 'Revenue grew' -o "$W/scan-redacted.pdf"
pdf-goat --agent search "$W/scan-redacted.pdf" "Revenue grew"
pdf-goat --agent get images "$W/scan-redacted.pdf" -o "$W/scan-images"
```

- `redactions` is 1 and `search` returns `count: 0`.
- `get images` writes the page images as stored: open the first with Read; the redacted words are blank in the image itself, so nothing survives under the box.
- OCR can misread a word, and a misread word escapes the pattern: compare `text` with a render before trusting the count.
