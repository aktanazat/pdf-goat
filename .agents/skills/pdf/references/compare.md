# Compare two versions

Set `A` to the earlier PDF, `B` to the later one and `W` to an empty work directory, all absolute. Text comparison says what words changed; visual comparison says where the page looks different, including changes text cannot see (images, layout, colour).

## 1. Text

```bash
pdf-goat --agent compare text "$A" "$B"
pdf-goat --agent compare text "$A" "$B" --mask '\d{3}-\d{2}-\d{4}'
```

- `identical`, `added`, `removed`, and `pages[]` with `page`, `match` (the `file` and `page` it was paired with, and the similarity `ratio`), `added`, `removed` and `diff` (unified diff lines: `-` from A, `+` from B, `@@` line numbers). Here `added: 1`, `removed: 1`: the amount changed from `1,200` to `1,350`.
- `unmatched` lists pages of the other files that no page of A matched (added pages). `diff_truncated: true` means more lines exist than `--max-lines` (default 20); raise it. `--context N` sets unchanged lines around each change.
- `--mask RE` replaces matches with `[REDACTED]` in the diff, for reports that must not repeat sensitive values.
- With several later files (`compare text A B C`) each page of A is paired with its closest page in any of them; `match.file` says which.

## 2. Visual

```bash
pdf-goat --agent compare visual "$A" "$B" --dpi 72 -o "$W/visual"
pdf-goat --agent render "$B" --pages 1 --dpi 144 --clip 50,180,420,215 --mark 149,194,161,203 -o "$W/look"
```

- `pages[]` with `page`, `changed_ratio` (share of pixels that differ; 0.0 is identical) and `bbox` of the changed area in pixels at `--dpi`, or null. `outputs` are diff images: changed pixels on black. Here page 1 changed 0.02% of its pixels, within `[149, 194, 161, 203]`: only the digits that differ.
- At `--dpi 72` the pixel `bbox` is already in points; at another dpi multiply by 72 / dpi. Then mark it on B, as above, and open both renders to say what changed.
- Small anti-aliasing differences give tiny nonzero ratios; judge by looking at the marked area.

## 3. Structure

```bash
pdf-goat --agent compare structure "$A" "$B"
```

- `identical`, `document` (each difference as `file` and `other` values, such as `page_count` and `object_count`) and `pages[]` with per-page `changes` (`size`, `rotation`, `annotations`, `widgets` and more). Use it to spot added fields, annotations, fonts or attachments that change no visible text; follow up with `form list`, `annotate list`, `get fonts` or `get attachments` on both files.
