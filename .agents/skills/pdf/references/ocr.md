# OCR a scan to a searchable PDF/A

Set `IN` to the scanned PDF and `W` to an empty work directory, both absolute. OCR runs on macOS Vision, so it works only on macOS, and takes up to about 15 seconds a page; run it in the background for long scans.

## 1. Confirm it needs OCR

```bash
pdf-goat --agent info "$IN"
```

- `has_text: false` means the pages are images only: `search`, `text` and `redact` find nothing until OCR. If `has_text` is true, OCR only with `--force` (it redoes the text layer).

## 2. OCR

```bash
pdf-goat --agent convert ocr "$IN" -o "$W/searchable.pdf"
```

- `standard` is `PDF/A-2u` (or `PDF/A-2b` when some text cannot map to Unicode, or null with a warning if the PDF/A step failed and the plain OCR result was kept). Conformance is not validated: say "saved as PDF/A-2u, not validated" when it matters.
- `warnings` must be empty or reported. One that says running OCR again moves a file aside means macOS compiled part of its text recognizer wrongly while the file was read, so long lines may be missing or misread: run the same command again, up to three more times while that warning comes back, and report it if it is still there. One that says a file was moved aside reports a repair made before reading, which adds about 15 seconds; the text of that run is good.

## 3. Check the text layer

```bash
pdf-goat --agent info "$W/searchable.pdf"
pdf-goat --agent text "$W/searchable.pdf"
pdf-goat --agent search "$W/searchable.pdf" "Revenue grew"
pdf-goat --agent render "$W/searchable.pdf" --pages 1 --dpi 144 --clip 50,95,450,135 --mark 141.8,110.5,218.1,124.2 -o "$W/look"
```

- `info` now reports `has_text: true`.
- `text` returns what Vision read, page by page. Compare it with words you can see on a render of the page; quote any misread word instead of correcting it silently.
- `search` for a phrase you can read on the page: here one hit on page 1 at `[141.8, 110.5, 218.1, 124.2]`. Render with `--mark` on that rect: the outline must sit on the printed words, which shows the invisible text lies over the image.
- Tables come out cell by cell in reading order; small or stylised characters (currency signs, lone digits) are the usual misreads.

The OCR'd file is the input for `redact.md` when a scan holds something to remove.
