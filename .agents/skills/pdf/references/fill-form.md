# Fill a form that has fields

Set `IN` to the form, `W` to an empty work directory, both absolute. A PDF with no fields is a paper form: use `sign-paper-form.md`, or add fields (section 6). The example is a one-page membership form with a name, a date and a check box.

## 1. Read the fields

```bash
pdf-goat --agent form list "$IN"
```

- `field_count` and `fields[]` with `name`, `type` (`Text`, `CheckBox`, `RadioButton`, `ComboBox`, `ListBox`, `Signature`), `value`, `page`, `rect` (search frame), `flags`, and for check boxes `on_state` and `checked`.
- `field_count: 0` means no fields. `xfa: true` means an XFA form, which pdf-goat does not support (preflight reports it too): stop and say so.
- Field names are often cryptic (`Text1`): find which label each belongs to by comparing its `rect` with `search` hits for the printed labels, or render the page with `--mark` on the field rects.

## 2. Write the values

A JSON object of field name to value. A check box takes its on state, with or without the slash (here `"Yes"` or `"/Yes"`), or `true`; `false`, `"Off"` and `""` clear it. A radio group takes the name of one of its states. Any other value for a button fails with `ok: false`, names the field's states, and writes no file.

```json $W/values.json
{"member_name": "Jane Q. Member", "date": "09/30/2026", "agree": "/Yes"}
```

## 3. Fill

```bash
pdf-goat --agent form fill "$IN" --data "$W/values.json" -o "$W/filled.pdf"
```

- `fields_set` lists the fields it set and `unknown_fields` the keys no field has. A misspelt name lands in `unknown_fields` while the command still succeeds, so check that it is empty. `flattened: false`.

## 4. Read it back and look

```bash
pdf-goat --agent form list "$W/filled.pdf"
pdf-goat --agent render "$W/filled.pdf" --pages 1 --dpi 144 --clip 50,100,400,190 --mark 146,110,280,125 --mark 312,162,380,177 --mark 62.5,141,70.5,149.5 -o "$W/look"
```

- Every field's `value` must equal what you meant: here `Jane Q. Member`, `09/30/2026`, and `agree` with `value` `Yes` and `checked: true`.
- Open `outputs[0]` with Read: each value sits inside its magenta field outline and the box shows a check. Text that overflows a narrow field is clipped on screen even when `value` is right.

## 5. Flatten, export, import

Flatten when the recipient must not change the values:

```bash
pdf-goat --agent form fill "$IN" --data "$W/values.json" --flatten -o "$W/flat.pdf"
pdf-goat --agent form list "$W/flat.pdf"
pdf-goat --agent search "$W/flat.pdf" "Jane Q. Member"
```

- `flattened: true`; `form list` then reports `field_count: 0`, and `search` finds the value as page text (here one hit, `[148.0, 109.5, 231.4, 124.9]`).

Move values between copies of a form:

```bash
pdf-goat --agent form export "$W/filled.pdf" --format xfdf -o "$W/values.xfdf"
pdf-goat --agent form import "$IN" --data "$W/values.xfdf" -o "$W/imported.pdf"
pdf-goat --agent form list "$W/imported.pdf"
```

- `form export` reports `format` and `field_count`; `--format json` writes the same values as section 2 (check boxes as `"/Yes"`), `fdf` an FDF file.
- `form import` takes JSON, XFDF or FDF and has `--flatten` too; confirm with `form list` as in section 4.

## 6. Add fields to a flat PDF

Set `FLAT` to a PDF without fields. Find each blank with `search` (underscore runs are text, so `search "$FLAT" "____"` finds the lines) and put a field over it:

```bash
pdf-goat --agent search "$FLAT" "____"
pdf-goat --agent form create-text "$FLAT" --name member_name --page 1 --rect 146,110,280,125 -o "$W/fields-1.pdf"
pdf-goat --agent form create-checkbox "$W/fields-1.pdf" --name agree --page 1 --rect 62.5,141,70.5,149.5 -o "$W/fillable.pdf"
pdf-goat --agent form list "$W/fillable.pdf"
```

- Here `search` finds three lines; the first, `[146.3, 112.4, 279.7, 124.4]`, is the name line.
- `form create-text` and `form create-checkbox` return the `field` name. `form list` must show both fields with the rects you gave; the check box has `on_state` `Yes`.

## Signing a filled form

Fill (and flatten, if wanted) before any certificate signature: a later fill shows as a modification in `security verify`, even on a form certified for filling. See `certificate-signing.md`.
