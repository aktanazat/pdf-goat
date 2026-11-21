# OCR a scan to a searchable PDF/A

`IN` the scan, `W` an empty work dir. macOS Vision only, up to ~15 s a page: run long scans in the background. A visual question needs no OCR: render and look.

## 1. Confirm it needs OCR

```bash
pdf-goat --agent info "$IN"
```

- `has_text: false` means the pages are images only: `search`, `text` and `redact` find nothing until OCR. If `has_text` is true, OCR only with `--force` (it redoes the text layer).

## 2. OCR

```bash
pdf-goat --agent convert ocr "$IN" -o "$W/searchable.pdf"
```

- `standard` `PDF/A-2u` (or `2b`, or null with a warning), not validated: say so.
- A warning that says running OCR again moves a file aside: lines may be missing; rerun the same command, up to three more times while it recurs, then report it. A warning that a file was moved aside alone is a repair; that run's text is good.

## 3. Check the text layer

```bash
pdf-goat --agent info "$W/searchable.pdf"
pdf-goat --agent text "$W/searchable.pdf"
pdf-goat --agent search "$W/searchable.pdf" "Revenue grew"
pdf-goat --agent render "$W/searchable.pdf" --pages 1 --dpi 144 --clip 50,95,450,135 --mark 141.8,110.5,218.1,124.2 -o "$W/look"
```

- Compare `text` with a render; quote misreads (currency signs, lone digits are common) instead of fixing them silently. The marked search rect must sit on the printed words.
