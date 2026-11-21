# Organize pages

`IN` the PDF, `OTHER` a second PDF, `W` an empty work dir. Prove order with `text`, count with `info`, rotation and size with `inspect`.

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

- `extract` keeps the order given. A partial `--order 3` moves page 3 first, rest in order. `--deg` is clockwise, a multiple of 90.

## 2. Insert, replace, blank, duplicate

```bash
pdf-goat --agent pages insert "$IN" --source "$OTHER" --at 2 -o "$W/inserted.pdf"
pdf-goat --agent pages replace "$IN" --source "$OTHER" --pages 3 -o "$W/replaced.pdf"
pdf-goat --agent text "$W/replaced.pdf"
pdf-goat --agent pages blank "$IN" --at 2 --count 1 -o "$W/blank.pdf"
pdf-goat --agent pages duplicate "$IN" --pages 1 -o "$W/dup.pdf"
```

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

- `crop --box` is top-left, y down, from the media box; search rects then shift with the crop. `boxes --rect` is raw PDF space (bottom-left). `booklet` pads with blanks.

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

- Headers, footers and Bates numbers are real page text (`search` finds them). `overlay` draws page 1 of the stamp PDF on every page. `--stamp 0` is "Approved" (0–13). `pages flatten` bakes annotations and fields into the page.
