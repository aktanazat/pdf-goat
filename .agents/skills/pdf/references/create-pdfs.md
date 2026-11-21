# Make PDFs from Markdown, HTML and images

`W` an empty work dir; `MD`, `CSS`, `HTML`, `IMG1`, `IMG2` absolute sources.

```bash
pdf-goat --agent from-md "$MD" --css "$CSS" -o "$W/memo.pdf"
pdf-goat --agent info "$W/memo.pdf"
pdf-goat --agent search "$W/memo.pdf" "Renew the office lease"
pdf-goat --agent get links "$W/memo.pdf"
pdf-goat --agent meta set "$W/memo.pdf" --set "title=Board Memo" -o "$W/memo-titled.pdf"
```

- `--css` replaces the default stylesheet. The title defaults to the file name: set a real one. Render and look before handing over.

```bash
pdf-goat --agent from-html "$HTML" -o "$W/flyer.pdf"
pdf-goat --agent inspect "$W/flyer.pdf"
pdf-goat --agent text "$W/flyer.pdf"
```

- The page's CSS (`@page { size }`, `page-break-before`) sets paper and breaks.

```bash
pdf-goat --agent from-images "$IMG1" "$IMG2" -o "$W/photos.pdf"
pdf-goat --agent inspect "$W/photos.pdf"
```

- One page per image; size follows pixels and stored dpi. No text layer: OCR if it must be searchable.

```bash
pdf-goat --agent convert pdfa "$W/memo-titled.pdf" -o "$W/memo-pdfa.pdf"
```

- Report "saved as PDF/A-2b, not validated".
