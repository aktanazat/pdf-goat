# Fill a form that has fields

`IN` the form, `W` an empty work dir. No fields: `sign-paper-form.md`, or add fields (section 6).

## 1. Read the fields

```bash
pdf-goat --agent form list "$IN"
```

- `fields[]`: `name`, `type`, `value`, `page`, `rect`, `flags`; check boxes add `on_state`, `checked`.
- `xfa: true`: unsupported; stop and say so.
- Cryptic names (`Text1`): match each `rect` to `search` hits for the printed labels, or render with `--mark`.

## 2. Write the values

Check box: its on state (`"Yes"` or `"/Yes"`) or `true`; `false`, `"Off"`, `""` clear. Radio: one of its state names. Other button values fail and write nothing.

```json $W/values.json
{"member_name": "Jane Q. Member", "date": "09/30/2026", "agree": "/Yes"}
```

## 3. Fill

```bash
pdf-goat --agent form fill "$IN" --data "$W/values.json" -o "$W/filled.pdf"
```

- `unknown_fields` must be empty: a misspelt name lands there while `ok` stays true.

## 4. Read it back and look

```bash
pdf-goat --agent form list "$W/filled.pdf"
pdf-goat --agent render "$W/filled.pdf" --pages 1 --dpi 144 --clip 50,100,400,190 --mark 146,110,280,125 --mark 312,162,380,177 --mark 62.5,141,70.5,149.5 -o "$W/look"
```

- Each `value` matches; the check box has `checked: true`. Text overflowing a narrow field is clipped on screen even when `value` is right.

## 5. Flatten, export, import

```bash
pdf-goat --agent form fill "$IN" --data "$W/values.json" --flatten -o "$W/flat.pdf"
pdf-goat --agent form list "$W/flat.pdf"
pdf-goat --agent search "$W/flat.pdf" "Jane Q. Member"
```

- `flattened: true`, then `field_count: 0`, and `search` finds the value as page text.

```bash
pdf-goat --agent form export "$W/filled.pdf" --format xfdf -o "$W/values.xfdf"
pdf-goat --agent form import "$IN" --data "$W/values.xfdf" -o "$W/imported.pdf"
pdf-goat --agent form list "$W/imported.pdf"
```

- Export formats `json|xfdf|fdf`; import takes any of them and `--flatten`.

## 6. Add fields to a flat PDF

```bash
pdf-goat --agent search "$FLAT" "____"
pdf-goat --agent form create-text "$FLAT" --name member_name --page 1 --rect 146,110,280,125 -o "$W/fields-1.pdf"
pdf-goat --agent form create-checkbox "$W/fields-1.pdf" --name agree --page 1 --rect 62.5,141,70.5,149.5 -o "$W/fillable.pdf"
pdf-goat --agent form list "$W/fillable.pdf"
```

- Underscore runs are text, so `search "____"` finds the blanks. `form list` must show the rects you gave; the check box's `on_state` is `Yes`.

Fill before any certificate signature (`certificate-signing.md`).
