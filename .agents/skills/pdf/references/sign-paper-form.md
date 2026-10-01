# Sign a paper form (no fields)

For a form whose blanks are printed lines and boxes: `form list` reports `field_count: 0`. Set `IN` to the form and `W` to an empty work directory, both absolute; `SIG` is the signer's signature image if there is one. This writes text on the page; it is not a certificate signature (`certificate-signing.md` adds one after this, if asked).

The example is a one-page A4 membership form with a name line, a check box, a signature line and a date line.

## 1. Find the blanks

```bash
pdf-goat --agent search "$IN" "____"
pdf-goat --agent search "$IN" "SIGNATURE:"
pdf-goat --agent search "$IN" "DATE:"
pdf-goat --agent search "$IN" "☐"
```

- Underscore runs are text, so `search "____"` returns every line: here the name line `[146.3, 112.4, 279.7, 124.4]`, the signature line `[138.0, 164.5, 271.5, 176.5]` and the date line `[312.6, 164.5, 379.3, 176.5]`. The labels tell you which is which: `SIGNATURE:` ends at x 134.7 and `DATE:` at 309.3.
- A printed box glyph (`☐`) is text too: here `[62.2, 136.4, 71.1, 152.5]`, the glyph's full height; the visible box is the middle of it, about `62.5,141,70.5,149.5`. A box drawn as lines is not text: find it on a render.
- The text baseline is about 2.5 pt above a rect's bottom at 12 pt.

Mark the targets and look before writing anything:

```bash
pdf-goat --agent render "$IN" --pages 1 --dpi 144 --clip 50,100,400,190 --mark 146.3,112.4,279.7,124.4 --mark 138.0,164.5,271.5,176.5 --mark 312.6,164.5,379.3,176.5 --mark 62.5,141,70.5,149.5 -o "$W/targets"
```

## 2. Write the name, the check, the signature, the date

```bash
pdf-goat --agent edit add-text "$IN" --text "Jane Q. Member" --at 150,121 --size 11 -o "$W/step1.pdf"
pdf-goat --agent edit add-text "$W/step1.pdf" --text 4 --font ZapfDingbats --fit --rect 62.5,141,70.5,149.5 --align center -o "$W/step2.pdf"
pdf-goat --agent edit add-text "$W/step2.pdf" --text "Jane Q. Member" --font "Snell Roundhand" --fit --rect 142,152,268,174 --size 22 --color "#0a1a5c" -o "$W/step3.pdf"
pdf-goat --agent edit add-text "$W/step3.pdf" --text 09/30/2026 --at 316,174 --size 11 -o "$W/signed.pdf"
```

- `--at x,y` is the start of the baseline. `--fit --rect` draws at the largest size that fits the box (at most `--size`), centred vertically, placed across by `--align`.
- Each call returns `page`, `bbox`, `placements[]` (one per page with `--pages`), `font` (the PostScript name) and `size`. Here: name `bbox` `[150.0, 109.2, 231.9, 124.3]`, Helvetica 11; check `[62.8, 140.7, 70.2, 149.6]`, ZapfDingbats 8.83; signature `[142.0, 152.0, 253.9, 174.0]`, SnellRoundhand 17.45; date `[316.0, 162.2, 371.0, 177.3]`.
- Each `bbox` must sit on its target: inside the line's x range and with its bottom near the line, or inside the box.
- ZapfDingbats maps ASCII to symbols: `4` is ✔, `8` is ✘, `l` is ●. Script fonts that ship with macOS: "Snell Roundhand", "Brush Script MT", "Apple Chancery". Any installed font name or a .ttf/.otf/.ttc file works; an unknown name fails with `font not found: …`.
- `--text` takes `\n` for a new line, `--width N` wraps, `--rotate` turns counter-clockwise, `--opacity` and `--color` (`#rrggbb`, a gray level, `r,g,b` or `c,m,y,k`) style it.

## 3. Read back and look

```bash
pdf-goat --agent search "$W/signed.pdf" "09/30/2026"
pdf-goat --agent render "$W/signed.pdf" --pages 1 --dpi 144 --clip 50,100,400,190 --mark 150.0,109.2,231.9,124.3 --mark 62.8,140.7,70.2,149.6 --mark 142.0,152.0,253.9,174.0 --mark 316.0,162.2,371.0,177.3 -o "$W/look"
```

- `search` finds the date once, at a rect inside the date's `bbox`: the text is real page text.
- Open `outputs[0]` with Read. Each magenta outline holds its text, the check sits inside the box, nothing overlaps a label. Move a misplaced item by redoing that step from the previous file with new numbers; never stack a correction on top.

## 4. A drawn signature instead of a script font

From an image (do this on `step2.pdf` instead of step 3):

```bash
pdf-goat --agent edit add-image "$W/step2.pdf" --image "$SIG" --rect 142,146,262,176 -o "$W/signed-image.pdf"
```

- `bbox` is where the image landed: it keeps its aspect ratio and is centred in `--rect` (`--stretch` fills the rect). Here the 4:1 image fills `[142.0, 146.0, 262.0, 176.0]`.
- Use a PNG with a transparent background: PNG transparency is kept, so the line shows through. A JPEG or an opaque PNG covers the line with a white box.

As ink, when you have the stroke as points (search frame) rather than an image:

```bash
pdf-goat --agent annotate ink "$W/step2.pdf" --page 1 --points "146,172;156,152;164,174;174,154;184,172;196,160;212,166;240,162" --color "#0a1a5c" --width 1.8 -o "$W/ink.pdf"
pdf-goat --agent annotate flatten "$W/ink.pdf" -o "$W/signed-ink.pdf"
pdf-goat --agent annotate list "$W/signed-ink.pdf"
```

- `annotate ink` returns `points` (here 8). `annotate flatten` bakes it into the page, so `annotate list` then reports `count: 0` and the stroke cannot be moved or deleted by the recipient.
- Render both with `--mark` on the target rect and look, as in section 3.
