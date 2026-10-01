# Organize pages

Set `IN` to the PDF, `OTHER` to a second PDF and `W` to an empty work directory, all absolute. The example `IN` has 3 A4 pages headed "Quarterly Report", "Second Page" and "Third Page"; `OTHER` is a one-page agreement. Pages are 1-based; ranges look like `2-5,9`.

After any reshuffle, prove the order with `text` (first line of each page) and the count with `info`; prove rotation and size with `inspect`.

## 1. Merge, split, extract, delete, reorder, rotate

```bash
pdf-goat --agent merge "$IN" "$OTHER" -o "$W/merged.pdf"
pdf-goat --agent info "$W/merged.pdf"
pdf-goat --agent split "$IN" --every 1 -o "$W/split"
pdf-goat --agent extract "$IN" --pages 3,1 -o "$W/extract.pdf"
pdf-goat --agent delete "$IN" --pages 2 -o "$W/deleted.pdf"
pdf-goat --agent reorder "$IN" --order 3,1,2 -o "$W/reordered.pdf"
pdf-goat --agent text "$W/reordered.pdf"
pdf-goat --agent rotate "$IN" --pages 2 --deg 90 -o "$W/rotated.pdf"
pdf-goat --agent inspect "$W/rotated.pdf"
```

- `merge`: `merged_pages` 4, and `info` agrees with `pages: 4`.
- `split`: `parts` 3, `outputs` `$W/split/report_001.pdf` to `_003.pdf`; `--every N` puts N pages in each part.
- `extract`: `pages` `[3, 1]`, kept in the order given.
- `delete`: `deleted_pages` `[2]`, `remaining_pages` 2.
- `reorder`: `order` `[3, 1, 2]`; `text` then starts the pages with "Third Page", "Quarterly Report", "Second Page". A partial `--order 3` moves page 3 first and keeps the rest in order.
- `rotate`: `rotated_pages` `[2]`, `deg` 90 (clockwise, a multiple of 90); `inspect` shows page 2 with `rotation: 90` and its displayed size swapped to 841.9 × 595.3.

## 2. Insert, replace, blank, duplicate

```bash
pdf-goat --agent pages insert "$IN" --source "$OTHER" --at 2 -o "$W/inserted.pdf"
pdf-goat --agent pages replace "$IN" --source "$OTHER" --pages 3 -o "$W/replaced.pdf"
pdf-goat --agent text "$W/replaced.pdf"
pdf-goat --agent pages blank "$IN" --at 2 --count 1 -o "$W/blank.pdf"
pdf-goat --agent pages duplicate "$IN" --pages 1 -o "$W/dup.pdf"
```

- `pages insert`: `at` 2, all of `OTHER` goes before page 2. `pages replace`: `replaced` `[3]`; `text` shows page 3 now starts "Service Agreement".
- `pages blank`: `inserted_at`, `inserted_pages`, `total_pages` 4. `pages duplicate`: `duplicated_pages` `[1]`, `total_pages` 4.

## 3. Size, crop, impose

```bash
pdf-goat --agent pages scale "$IN" --factor 0.5 -o "$W/scaled.pdf"
pdf-goat --agent pages crop "$IN" --box 36,36,559,806 --pages 1 -o "$W/cropped.pdf"
pdf-goat --agent inspect "$W/cropped.pdf"
pdf-goat --agent pages boxes "$IN" --box trim --rect 10,10,585,831 -o "$W/trimmed.pdf"
pdf-goat --agent get object "$W/trimmed.pdf" page:1
pdf-goat --agent pages nup "$IN" --n 2 -o "$W/nup.pdf"
pdf-goat --agent pages booklet "$IN" -o "$W/booklet.pdf"
```

- `pages scale`: `factor` 0.5; pages become 297.6 × 420.9.
- `pages crop --box` is top-left, y down, from the media box: `inspect` shows page 1 at 523 × 770. The search frame moves with the crop box, so search rects on page 1 shift by 36 pt.
- `pages boxes --rect` is raw PDF space (bottom-left origin): `get object … page:1` shows `/TrimBox [ 10 10 585 831 ]`.
- `pages nup --n 2|4`: `sheets` 2 (two A4 pages side by side on 1190.6 × 841.9). `pages booklet`: `page_order` `[4, 1, 2, 3]` and `sheets` 2; page 4 is a blank added to fill the sheet.

## 4. Headers, numbers, stamps

```bash
pdf-goat --agent pages header "$IN" --text "Quarterly Report" --align left -o "$W/header.pdf"
pdf-goat --agent pages footer "$W/header.pdf" --text "Page {page} of {pages}" -o "$W/footer.pdf"
pdf-goat --agent search "$W/footer.pdf" "Page 2 of 3"
pdf-goat --agent pages numbers "$IN" --format "{page}/{pages}" --align right -o "$W/numbered.pdf"
pdf-goat --agent pages bates "$IN" --prefix ACME- --start 100 --digits 5 -o "$W/bates.pdf"
pdf-goat --agent search "$W/bates.pdf" "ACME-00102"
pdf-goat --agent watermark "$IN" --text DRAFT --opacity 0.2 -o "$W/draft.pdf"
pdf-goat --agent overlay "$IN" "$OTHER" -o "$W/overlaid.pdf"
pdf-goat --agent annotate stamp "$IN" --page 1 --rect 400,60,560,110 --stamp 0 -o "$W/approved.pdf"
pdf-goat --agent pages flatten "$W/approved.pdf" -o "$W/approved-flat.pdf"
pdf-goat --agent annotate list "$W/approved-flat.pdf"
```

- Header and footer text is real page text: `search` finds "Page 2 of 3" once, on page 2, near the bottom (`[272.1, 795.1, 323.2, 808.9]`).
- `pages bates`: `first` `ACME-00100`; `search` finds `ACME-00102` on page 3.
- `watermark` puts the text diagonally on every page (`search "DRAFT"` hits all 3); render a page to judge `--opacity`.
- `overlay` draws page 1 of the stamp PDF over every page of `IN`.
- `annotate stamp --stamp 0` is the "Approved" stamp (0 to 13 are predefined). `pages flatten` bakes annotations and form fields into the page: `flattened` `["annotations", "form_fields"]`, then `annotate list` `count: 0`.
