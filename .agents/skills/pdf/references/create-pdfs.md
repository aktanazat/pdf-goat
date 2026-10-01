# Make PDFs from Markdown, HTML and images

Set `W` to an empty work directory and the sources as absolute paths: `MD` a Markdown file and `CSS` a stylesheet, `HTML` a web page, `IMG1` and `IMG2` PNG or JPEG images. To write on an existing page instead, use `edit add-text` (`sign-paper-form.md`).

## 1. Markdown

```bash
pdf-goat --agent from-md "$MD" --css "$CSS" -o "$W/memo.pdf"
pdf-goat --agent info "$W/memo.pdf"
pdf-goat --agent search "$W/memo.pdf" "Renew the office lease"
pdf-goat --agent get links "$W/memo.pdf"
pdf-goat --agent meta set "$W/memo.pdf" --set "title=Board Memo" -o "$W/memo-titled.pdf"
```

- `from-md` returns `output_bytes`. Headings, lists, tables and links come through; `--css` replaces the default stylesheet.
- `info`: `pages` 1, and `metadata.title` is the file name (`memo`), so set a real title with `meta set`.
- `search` finds a sentence from the source once (here `[92.3, 181.9, 202.3, 194.4]`); `get links` lists the Markdown link with its `uri` (`mailto:secretary@example.com`) and rect.
- Render page 1 and look at fonts, table borders and colours before handing it over.

## 2. HTML

```bash
pdf-goat --agent from-html "$HTML" -o "$W/flyer.pdf"
pdf-goat --agent inspect "$W/flyer.pdf"
pdf-goat --agent text "$W/flyer.pdf"
```

- The page's CSS sets the paper: here `@page { size: Letter }` gives 612 × 792 pt pages, and `page-break-before: always` starts page 2, which `text` shows beginning "Directions".
- Check `inspect` `total_pages` and each page's size against what was asked.

## 3. Images

```bash
pdf-goat --agent from-images "$IMG1" "$IMG2" -o "$W/photos.pdf"
pdf-goat --agent inspect "$W/photos.pdf"
```

- `image_count` 2, one page per image in the order given. The page size follows the image's pixels and stored resolution: these 596 × 842 px images at 72 dpi make 596 × 842 pt pages; a 200 dpi A4 scan makes an A4 page.
- The pages hold pictures only (`info` `has_text: false`): OCR them (`ocr.md`) when the text must be searchable.

## 4. Archive copy

```bash
pdf-goat --agent convert pdfa "$W/memo-titled.pdf" -o "$W/memo-pdfa.pdf"
```

- `standard` `PDF/A-2b` and `conformance_validated: false`, with a `note` saying so: report "saved as PDF/A-2b, not validated".
