use std::ffi::OsString;
use std::path::Path;

use goat_common::{Ctx, GoatError, Registry};
use goat_fixtures::PdfBuilder;
use pdf_core::{Document, Object};
use serde_json::{Map, Value, json};

fn run(home: &Path, args: &[&str]) -> Result<Map<String, Value>, GoatError> {
    let mut registry = Registry::new();
    pdf_forms::register(&mut registry);
    let cli = registry.into_cli(
        clap::Command::new("pdf-goat"),
        &["form", "annotate", "pages"],
        &[],
    );
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    cli.parse(&args)
        .unwrap_or_else(|failure| panic!("{failure:?}"))
        .run(&Ctx::new(home))
}
fn fixture(home: &Path) -> String {
    let mut builder = PdfBuilder::new();
    builder
        .page(595.0, 842.0)
        .text_field("name", [72.0, 72.0, 272.0, 92.0], "Alice")
        .text_field("shared", [72.0, 120.0, 272.0, 140.0], "same")
        .text_field("shared", [72.0, 160.0, 272.0, 180.0], "same")
        .checkbox("agree", [72.0, 200.0, 92.0, 220.0], true)
        .combo(
            "color",
            [72.0, 240.0, 272.0, 260.0],
            &["Red", "Green", "Blue"],
            "Green",
        );
    builder
        .save(home.join("form.pdf"))
        .to_string_lossy()
        .into_owned()
}
fn output(result: &Map<String, Value>) -> &str {
    result["outputs"][0].as_str().expect("output path")
}
/// A lone check box `agree` with the states `Yes` and `Off`.
fn checkbox_fixture(home: &Path, checked: bool) -> String {
    let mut builder = PdfBuilder::new();
    builder
        .page(595.0, 842.0)
        .checkbox("agree", [72.0, 200.0, 92.0, 220.0], checked);
    builder
        .save(home.join("checkbox.pdf"))
        .to_string_lossy()
        .into_owned()
}
/// A radio group `size` whose separate parent holds two kids with the on states
/// `S` and `L`; `S` is selected.
fn radio_fixture(home: &Path) -> String {
    let mut builder = PdfBuilder::new();
    builder
        .page(595.0, 842.0)
        .checkbox("size", [72.0, 72.0, 92.0, 92.0], true)
        .checkbox("size", [112.0, 72.0, 132.0, 92.0], false);
    let source = builder
        .save(home.join("radio.pdf"))
        .to_string_lossy()
        .into_owned();
    let mut doc = Document::open(&source).expect("fixture");
    let mut parent_id = None;
    for (widget, state) in pdf_forms::page_widgets(&doc, 0)
        .expect("widgets")
        .into_iter()
        .zip(["S", "L"])
    {
        let mut kid = doc
            .get(widget)
            .expect("kid")
            .as_dict()
            .expect("dict")
            .clone();
        let mut ap = doc
            .resolve_key(&kid, b"AP")
            .expect("appearance")
            .as_dict()
            .expect("dict")
            .clone();
        let mut normal = doc
            .resolve_key(&ap, b"N")
            .expect("states")
            .as_dict()
            .expect("dict")
            .clone();
        let on = normal.remove(b"Yes").expect("on appearance");
        normal.insert(state, on);
        ap.insert("N", normal);
        kid.insert("AP", ap);
        kid.insert("AS", Object::name(if state == "S" { "S" } else { "Off" }));
        parent_id = kid.get_ref(b"Parent");
        doc.set(widget, kid);
    }
    let parent_id = parent_id.expect("separate parent field");
    let mut parent = doc
        .get(parent_id)
        .expect("parent")
        .as_dict()
        .expect("dict")
        .clone();
    parent.insert("Ff", 49152_i64);
    parent.insert("V", Object::name("S"));
    doc.set(parent_id, parent);
    doc.save(&source, &pdf_core::SaveOptions::default())
        .expect("save");
    source
}
fn fill_json(home: &Path, source: &str, data: &str) -> Result<Map<String, Value>, GoatError> {
    let path = home.join("data.json");
    std::fs::write(&path, data).expect("data");
    run(
        home,
        &[
            "form",
            "fill",
            source,
            "--data",
            path.to_str().expect("path"),
        ],
    )
}
/// The listed widgets' `checked` flags, and the exported field values, of a filled file.
fn button_states(home: &Path, filled: &Map<String, Value>) -> (Vec<Value>, Value) {
    let listed = run(home, &["form", "list", output(filled)]).expect("list");
    let checked = listed["fields"]
        .as_array()
        .expect("fields")
        .iter()
        .map(|field| field["checked"].clone())
        .collect();
    let export = run(home, &["form", "export", output(filled)]).expect("export");
    let exported = serde_json::from_slice(&std::fs::read(output(&export)).expect("export file"))
        .expect("json");
    (checked, exported)
}

#[test]
fn list_preserves_widget_instances_and_inherited_values() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = fixture(home.path());
    let result = run(home.path(), &["form", "list", &source]).expect("list");
    assert_eq!(
        result["fields"],
        json!([
            {"name":"name","type":"Text","value":"Alice","page":1,"rect":[72.0,72.0,272.0,92.0],"flags":0},
            {"name":"shared","type":"Text","value":"same","page":1,"rect":[72.0,120.0,272.0,140.0],"flags":0},
            {"name":"shared","type":"Text","value":"same","page":1,"rect":[72.0,160.0,272.0,180.0],"flags":0},
            {"name":"agree","type":"CheckBox","value":"Yes","page":1,"rect":[72.0,200.0,92.0,220.0],"flags":0,"on_state":"Yes","checked":true},
            {"name":"color","type":"ComboBox","value":"Green","page":1,"rect":[72.0,240.0,272.0,260.0],"flags":131072,"options":["Red","Green","Blue"]}
        ])
    );
    let export = run(home.path(), &["form", "export", &source]).expect("export");
    let data: Value = serde_json::from_slice(&std::fs::read(output(&export)).expect("export file"))
        .expect("json");
    assert_eq!(
        data,
        json!({"name":"Alice","shared":"same","agree":"/Yes","color":"Green"})
    );
}

#[test]
fn fill_updates_both_shared_widgets_and_keeps_values_after_flattening() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = fixture(home.path());
    let data = home.path().join("data.json");
    std::fs::write(
        &data,
        br#"{"name":"Bob","shared":"Together","agree":"/Yes","color":"Blue"}"#,
    )
    .expect("data");
    let filled = run(
        home.path(),
        &[
            "form",
            "fill",
            &source,
            "--data",
            data.to_str().expect("path"),
        ],
    )
    .expect("fill");
    let listed = run(home.path(), &["form", "list", output(&filled)]).expect("list filled");
    assert_eq!(listed["fields"][0]["value"], "Bob");
    assert_eq!(listed["fields"][1]["value"], "Together");
    assert_eq!(listed["fields"][2]["value"], "Together");
    assert_eq!(listed["fields"][3]["checked"], true);
    assert_eq!(listed["fields"][4]["value"], "Blue");
    let flat = run(
        home.path(),
        &[
            "form",
            "fill",
            &source,
            "--data",
            data.to_str().expect("path"),
            "--flatten",
            "-o",
            home.path().join("flat.pdf").to_str().expect("path"),
        ],
    )
    .expect("flatten");
    let doc = Document::open(output(&flat)).expect("flattened PDF");
    assert!(!doc.catalog().expect("catalog").contains_key(b"AcroForm"));
    assert!(
        pdf_forms::page_widgets(&doc, 0)
            .expect("widgets")
            .is_empty()
    );
    let page = doc.page(0).expect("page");
    let objects = doc
        .resolve_key(&page.resources, b"XObject")
        .expect("xobjects");
    let mut drawn = String::new();
    for (_, object) in objects.as_dict().expect("resource dict").iter() {
        let stream = doc
            .resolve_stream(object)
            .expect("resolve")
            .expect("appearance");
        drawn.push_str(&String::from_utf8_lossy(
            &doc.decode_stream(&stream).expect("decode").data,
        ));
    }
    assert!(drawn.contains("(Bob) Tj"));
    assert_eq!(drawn.matches("(Together) Tj").count(), 2);
    assert!(drawn.contains("(Blue) Tj"));
}

/// Each case fills `agree` from a box that starts `checked` and expects the box's
/// state and its exported value: a name object, never a text string.
macro_rules! checkbox_fill_cases {
    ($($test:ident: $checked:expr, $data:expr => $now:expr, $exported:expr;)*) => {$(
        #[test]
        fn $test() {
            let home = tempfile::tempdir().expect("tempdir");
            let source = checkbox_fixture(home.path(), $checked);
            let filled = fill_json(home.path(), &source, $data).expect("fill");
            assert_eq!(
                button_states(home.path(), &filled),
                (vec![json!($now)], json!({ "agree": $exported }))
            );
        }
    )*};
}
checkbox_fill_cases! {
    checkbox_fill_with_a_bare_state_name_checks_the_box: false, r#"{"agree":"Yes"}"# => true, "/Yes";
    checkbox_fill_with_a_slashed_state_name_checks_the_box: false, r#"{"agree":"/Yes"}"# => true, "/Yes";
    checkbox_fill_with_true_checks_the_box: false, r#"{"agree":true}"# => true, "/Yes";
    checkbox_fill_with_off_clears_the_box: true, r#"{"agree":"Off"}"# => false, "/Off";
    checkbox_fill_with_false_clears_the_box: true, r#"{"agree":false}"# => false, "/Off";
}

#[test]
fn checkbox_fill_with_an_unknown_state_fails_and_names_the_states() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = checkbox_fixture(home.path(), false);
    let error = fill_json(home.path(), &source, r#"{"agree":"On"}"#).expect_err("unknown state");
    assert_eq!(
        error.to_string(),
        "form field 'agree' has no state 'On'; its states are: Yes, Off"
    );
}

#[test]
fn radio_fill_selects_one_kid_and_stores_the_state_on_the_parent() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = radio_fixture(home.path());
    let filled = fill_json(home.path(), &source, r#"{"size":"L"}"#).expect("fill");
    assert_eq!(
        button_states(home.path(), &filled),
        (vec![json!(false), json!(true)], json!({"size": "/L"}))
    );
}

#[test]
fn radio_fill_refuses_true_as_ambiguous() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = radio_fixture(home.path());
    let error = fill_json(home.path(), &source, r#"{"size":true}"#).expect_err("ambiguous");
    assert_eq!(
        error.to_string(),
        "form field 'size' is a radio group, so true is ambiguous; give one of its states: S, L, Off"
    );
}

#[test]
fn xfdf_import_checks_a_box_from_a_state_without_a_slash() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = checkbox_fixture(home.path(), false);
    let data = home.path().join("data.xfdf");
    std::fs::write(&data, r#"<xfdf xmlns="http://ns.adobe.com/xfdf/"><fields><field name="agree"><value>Yes</value></field></fields></xfdf>"#).expect("data");
    let filled = run(
        home.path(),
        &[
            "form",
            "import",
            &source,
            "--data",
            data.to_str().expect("path"),
        ],
    )
    .expect("import");
    assert_eq!(
        button_states(home.path(), &filled),
        (vec![json!(true)], json!({"agree": "/Yes"}))
    );
}

#[test]
fn fill_reports_misspelled_field_names_apart_from_the_fields_it_set() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = fixture(home.path());
    let filled = fill_json(
        home.path(),
        &source,
        r#"{"nmae":"Bob","color":"Red","agre":true}"#,
    )
    .expect("fill still succeeds");
    assert_eq!(
        (&filled["fields_set"], &filled["unknown_fields"]),
        (&json!(["color"]), &json!(["nmae", "agre"]))
    );
}

#[test]
fn xfdf_import_decodes_entities_and_last_duplicate_wins() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = fixture(home.path());
    let data = home.path().join("data.xfdf");
    std::fs::write(&data,r#"<xfdf xmlns="http://ns.adobe.com/xfdf/"><fields><field name="name"><value>old</value></field><field name="color"><value>Blue</value></field><field name="name"><value>A &amp; B</value></field></fields></xfdf>"#).expect("data");
    let filled = run(
        home.path(),
        &[
            "form",
            "import",
            &source,
            "--data",
            data.to_str().expect("path"),
        ],
    )
    .expect("import");
    assert_eq!(
        (&filled["fields_set"], &filled["unknown_fields"]),
        (&json!(["name", "color"]), &json!([]))
    );
    let listed = run(home.path(), &["form", "list", output(&filled)]).expect("list");
    assert_eq!(listed["fields"][0]["value"], "A & B");
    assert_eq!(listed["fields"][4]["value"], "Blue");
}

#[test]
fn widget_appearance_update_does_not_erase_existing_value() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = fixture(home.path());
    let mut doc = Document::open(source).expect("fixture");
    let id = pdf_forms::page_widgets(&doc, 0).expect("widgets")[0];
    pdf_forms::update_widget_appearance(&mut doc, id).expect("update");
    let object = doc.get(id).expect("field");
    let field = object.as_dict().expect("dict");
    assert_eq!(
        pdf_forms::field_value_text(&doc, field).expect("value"),
        "Alice"
    );
    let ap = doc.resolve_key(field, b"AP").expect("ap");
    let normal = doc
        .resolve_key(ap.as_dict().expect("dict"), b"N")
        .expect("normal");
    assert!(matches!(normal, Object::Stream(_)));
}

#[test]
fn pages_flatten_generates_missing_appearances_before_removing_fields() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = fixture(home.path());
    let mut doc = Document::open(&source).expect("fixture");
    let catalog_id = doc.catalog_ref().expect("catalog");
    let mut catalog = doc.catalog().expect("catalog");
    let acro_object = catalog.get(b"AcroForm").expect("form").clone();
    let mut acro = doc
        .resolve_dict(&acro_object)
        .expect("resolve")
        .expect("dictionary");
    acro.insert("NeedAppearances", true);
    if let Some(id) = acro_object.as_reference() {
        doc.set(id, acro);
    } else {
        catalog.insert("AcroForm", acro);
        doc.set(catalog_id, catalog);
    }
    for widget in pdf_forms::page_widgets(&doc, 0).expect("widgets") {
        let mut field = doc
            .get(widget)
            .expect("widget")
            .as_dict()
            .expect("dict")
            .clone();
        if pdf_forms::widget_type(&doc, &field).expect("type") == pdf_forms::WidgetType::Text {
            field.remove(b"AP");
            doc.set(widget, field);
        }
    }
    doc.save(&source, &pdf_core::SaveOptions::default())
        .expect("save");
    let flat = run(home.path(), &["pages", "flatten", &source]).expect("flatten");
    let doc = Document::open(output(&flat)).expect("flattened");
    assert!(!doc.catalog().expect("catalog").contains_key(b"AcroForm"));
    assert!(
        pdf_forms::page_widgets(&doc, 0)
            .expect("widgets")
            .is_empty()
    );
    let text = pdf_text::extract_page(&doc, 0, pdf_text::TextFlags::TEXT)
        .expect("extract")
        .text();
    assert!(text.contains("Alice"), "{text}");
    assert_eq!(text.matches("same").count(), 2, "{text}");
    assert!(text.contains("Green"), "{text}");
}

#[test]
fn clearing_shared_fields_removes_live_default_rich_and_old_appearance_values() {
    let home = tempfile::tempdir().expect("tempdir");
    let source = fixture(home.path());
    let mut doc = Document::open(&source).expect("fixture");
    let widgets = pdf_forms::page_widgets(&doc, 0).expect("widgets");
    let child = doc
        .get(widgets[1])
        .expect("widget")
        .as_dict()
        .expect("dictionary")
        .clone();
    let parent_id = child.get_ref(b"Parent").expect("shared parent");
    let mut parent = doc
        .get(parent_id)
        .expect("parent")
        .as_dict()
        .expect("dictionary")
        .clone();
    parent.insert("V", Object::text("live-secret"));
    let reset = doc.add(Object::text("reset-secret"));
    parent.insert("DV", Object::Reference(reset));
    let rich = doc.add(pdf_core::Stream::new(
        pdf_core::Dict::new(),
        b"<p>rich-secret</p>".to_vec(),
    ));
    parent.insert("RV", Object::Reference(rich));
    doc.set(parent_id, parent);
    for id in [widgets[1], widgets[2]] {
        let mut field = doc.get(id).expect("field").as_dict().expect("dict").clone();
        let mut ap = doc
            .resolve_key(&field, b"AP")
            .expect("appearance")
            .as_dict()
            .expect("dict")
            .clone();
        let stale = doc.add(pdf_core::Stream::new(
            pdf_core::Dict::new(),
            b"(appearance-secret) Tj".to_vec(),
        ));
        ap.insert("D", Object::Reference(stale));
        field.insert("AP", ap);
        doc.set(id, field);
    }
    pdf_forms::clear_field_value(&mut doc, widgets[1]).expect("clear");
    let bytes = doc
        .save_to_bytes(&pdf_core::SaveOptions {
            garbage_collect: true,
            ..Default::default()
        })
        .expect("full save");
    let doc = Document::load(bytes).expect("reopen");
    let mut shared = 0;
    for id in pdf_forms::page_widgets(&doc, 0).expect("widgets") {
        let field = doc.get(id).expect("field").as_dict().expect("dict").clone();
        match pdf_forms::field_full_name(&doc, &field)
            .expect("name")
            .as_str()
        {
            "shared" => {
                assert_eq!(
                    pdf_forms::field_value_text(&doc, &field).expect("value"),
                    ""
                );
                assert!(
                    !doc.resolve_key(&field, b"AP")
                        .expect("appearance")
                        .is_null()
                );
                shared += 1;
            }
            "name" => assert_eq!(
                pdf_forms::field_value_text(&doc, &field).expect("value"),
                "Alice"
            ),
            _ => {}
        }
    }
    assert_eq!(shared, 2);
    for id in doc.object_ids() {
        let object = doc.get(id).expect("object");
        let bytes = if let Some(stream) = object.as_stream() {
            doc.decode_stream(stream).expect("decode").data
        } else {
            object.to_bytes()
        };
        let data = String::from_utf8_lossy(&bytes);
        for secret in [
            "live-secret",
            "reset-secret",
            "rich-secret",
            "appearance-secret",
        ] {
            assert!(!data.contains(secret), "{secret} remains in {id:?}");
        }
    }
}
