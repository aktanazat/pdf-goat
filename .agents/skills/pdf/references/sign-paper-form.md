# Sign a paper form (no fields)

`form list` reports `field_count: 0`. `IN` the form, `W` an empty work dir, `SIG` an optional signature image. This writes page text, not a certificate signature.

## 1. Find the blanks

```bash
pdf-goat --agent search "$IN" "____"
pdf-goat --agent search "$IN" "SIGNATURE:"
pdf-goat --agent search "$IN" "DATE:"
pdf-goat --agent search "$IN" "☐"
```

- Underscore lines and `☐` glyphs are text; labels tell which line is which. A glyph rect spans its full height: the visible box is its middle. A box drawn as lines is not text: find it on a render.
- Baseline sits ~2.5 pt above a rect's bottom at 12 pt.

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

- `--at x,y` is the baseline start. `--fit --rect` uses the largest size that fits (at most `--size`).
- Each returns `bbox`, `font`, `size`: the `bbox` must sit on its line or inside its box.
- ZapfDingbats: `4` ✔, `8` ✘, `l` ●. macOS script fonts: "Snell Roundhand", "Brush Script MT", "Apple Chancery"; a .ttf/.otf path also works.
- `--text` takes `\n`; `--width` wraps; `--color` takes `#rrggbb`, gray, `r,g,b`, `c,m,y,k`.

## 3. Read back and look

```bash
pdf-goat --agent search "$W/signed.pdf" "09/30/2026"
pdf-goat --agent render "$W/signed.pdf" --pages 1 --dpi 144 --clip 50,100,400,190 --mark 150.0,109.2,231.9,124.3 --mark 62.8,140.7,70.2,149.6 --mark 142.0,152.0,253.9,174.0 --mark 316.0,162.2,371.0,177.3 -o "$W/look"
```

- Fix a misplaced item by redoing that step from the previous file; never stack a correction.

## 4. Drawn signature

Image, on `step2.pdf` instead of step 3:

```bash
pdf-goat --agent edit add-image "$W/step2.pdf" --image "$SIG" --rect 142,146,262,176 -o "$W/signed-image.pdf"
```

- Keeps aspect ratio, centred (`--stretch` fills). Use a transparent PNG; JPEG or opaque PNG covers the line.

Ink from points (search frame):

```bash
pdf-goat --agent annotate ink "$W/step2.pdf" --page 1 --points "146,172;156,152;164,174;174,154;184,172;196,160;212,166;240,162" --color "#0a1a5c" --width 1.8 -o "$W/ink.pdf"
pdf-goat --agent annotate flatten "$W/ink.pdf" -o "$W/signed-ink.pdf"
pdf-goat --agent annotate list "$W/signed-ink.pdf"
```

- After `annotate flatten`, `annotate list` shows `count: 0`.
