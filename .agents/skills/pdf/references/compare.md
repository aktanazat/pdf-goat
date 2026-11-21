# Compare two versions

`A` earlier, `B` later, `W` an empty work dir.

```bash
pdf-goat --agent compare text "$A" "$B"
pdf-goat --agent compare text "$A" "$B" --mask '\d{3}-\d{2}-\d{4}'
```

- `pages[].diff` is unified diff; `unmatched` lists added pages; `diff_truncated` means raise `--max-lines` (default 20). Several later files: each page of A pairs with its closest page anywhere (`match.file`).

```bash
pdf-goat --agent compare visual "$A" "$B" --dpi 72 -o "$W/visual"
pdf-goat --agent render "$B" --pages 1 --dpi 144 --clip 50,180,420,215 --mark 149,194,161,203 -o "$W/look"
```

- `bbox` is pixels at `--dpi` (at 72 it equals points). Mark it on B and look; tiny ratios are often anti-aliasing.

```bash
pdf-goat --agent compare structure "$A" "$B"
```

- Spots added fields, annotations, fonts, attachments that change no visible text.
