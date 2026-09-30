use std::ffi::OsString;
use std::io::{Cursor, Read};

use goat_common::{Ctx, Registry};
use goat_fixtures::{Paint, PdfBuilder};
use roxmltree::{Document as Xml, Node, NodeId};
use zip::ZipArchive;

const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

fn convert(builder: PdfBuilder) -> ZipArchive<Cursor<Vec<u8>>> {
    let home = tempfile::tempdir().expect("temporary home");
    let source = builder.save(home.path().join("source.pdf"));
    let mut registry = Registry::new();
    pdf_edit::register(&mut registry);
    let cli = registry.into_cli(
        clap::Command::new("pdf-goat"),
        &["convert", "edit", "redact"],
        &[],
    );
    let args = [
        OsString::from("convert"),
        OsString::from("docx"),
        source.into_os_string(),
    ];
    let result = cli
        .parse(&args)
        .expect("parse")
        .run(&Ctx::new(home.path()))
        .expect("convert");
    let bytes =
        std::fs::read(result["outputs"][0].as_str().expect("output path")).expect("Office package");
    ZipArchive::new(Cursor::new(bytes)).expect("independent ZIP reader")
}

fn member(archive: &mut ZipArchive<Cursor<Vec<u8>>>, name: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    archive
        .by_name(name)
        .expect("referenced part")
        .read_to_end(&mut bytes)
        .expect("read part");
    bytes
}

fn document_part(archive: &mut ZipArchive<Cursor<Vec<u8>>>) -> String {
    let bytes = member(archive, "_rels/.rels");
    let xml = Xml::parse(std::str::from_utf8(&bytes).expect("relationships UTF-8"))
        .expect("relationships XML");
    xml.descendants()
        .find(|node| {
            node.attribute("Type")
                .is_some_and(|kind| kind.ends_with("/officeDocument"))
        })
        .and_then(|node| node.attribute("Target"))
        .expect("main document relationship")
        .trim_start_matches('/')
        .to_owned()
}

fn paragraph_text(node: Node<'_, '_>) -> String {
    let mut text = String::new();
    for node in node.descendants() {
        if node.has_tag_name((W, "t")) {
            text.push_str(node.text().unwrap_or(""));
        } else if node.has_tag_name((W, "br")) {
            text.push('\n');
        } else if node.has_tag_name((W, "tab")) {
            text.push('\t');
        }
    }
    text
}

#[test]
fn nearby_lines_form_editable_paragraphs_without_joining_columns_or_separate_paragraphs() {
    let mut fixture = PdfBuilder::new();
    let page = fixture.page(600.0, 800.0);
    for (x, prefix) in [(48.0, "left"), (320.0, "right")] {
        page.text(x, 100.0, &format!("{prefix} one alpha"))
            .text(x, 116.0, &format!("{prefix} two beta"))
            .text(x, 180.0, &format!("{prefix} new paragraph"));
    }
    let mut archive = convert(fixture);
    let part = document_part(&mut archive);
    let bytes = member(&mut archive, &part);
    let document =
        Xml::parse(std::str::from_utf8(&bytes).expect("document UTF-8")).expect("document XML");
    let paragraphs: Vec<_> = document
        .descendants()
        .filter(|node| node.has_tag_name((W, "p")))
        .map(paragraph_text)
        .filter(|text| !text.is_empty())
        .collect();
    assert_eq!(
        paragraphs,
        [
            "left one alpha\nleft two beta",
            "right one alpha\nright two beta",
            "left new paragraph",
            "right new paragraph"
        ]
    );
}

#[test]
fn merged_cells_reopen_as_shared_logical_cells_in_both_directions() {
    let mut fixture = PdfBuilder::new();
    let page = fixture.page(600.0, 800.0);
    let rule = Paint::stroke([0.0; 3], 1.0);
    for y in [100.0, 140.0, 220.0] {
        page.line((60.0, y), (510.0, y), rule);
    }
    page.line((210.0, 180.0), (510.0, 180.0), rule);
    for x in [60.0, 360.0, 510.0] {
        page.line((x, 100.0), (x, 220.0), rule);
    }
    page.line((210.0, 140.0), (210.0, 220.0), rule)
        .text(70.0, 124.0, "horizontal")
        .text(370.0, 124.0, "top right")
        .text(70.0, 165.0, "vertical")
        .text(220.0, 165.0, "center")
        .text(370.0, 165.0, "right")
        .text(220.0, 205.0, "lower")
        .text(370.0, 205.0, "end");
    let mut archive = convert(fixture);
    let part = document_part(&mut archive);
    let bytes = member(&mut archive, &part);
    let document =
        Xml::parse(std::str::from_utf8(&bytes).expect("document UTF-8")).expect("document XML");
    let table = document
        .descendants()
        .find(|node| node.has_tag_name((W, "tbl")))
        .expect("editable table");
    let mut grid: Vec<Vec<NodeId>> = Vec::new();
    for row in table.children().filter(|node| node.has_tag_name((W, "tr"))) {
        let mut logical = Vec::new();
        for cell in row.children().filter(|node| node.has_tag_name((W, "tc"))) {
            let span = cell
                .descendants()
                .find(|node| node.has_tag_name((W, "gridSpan")))
                .and_then(|node| node.attribute((W, "val")))
                .map_or(1, |value| value.parse::<usize>().expect("column span"));
            let continuation = cell
                .descendants()
                .find(|node| node.has_tag_name((W, "vMerge")))
                .is_some_and(|node| node.attribute((W, "val")) != Some("restart"));
            let owner = if continuation {
                grid.last().expect("merge starts above")[logical.len()]
            } else {
                cell.id()
            };
            logical.extend(std::iter::repeat_n(owner, span));
        }
        grid.push(logical);
    }
    assert_eq!(grid.iter().map(Vec::len).collect::<Vec<_>>(), [3, 3, 3]);
    assert_eq!(grid[0][0], grid[0][1]);
    assert_eq!(grid[1][0], grid[2][0]);
    assert_ne!(grid[0][0], grid[0][2]);
    assert_ne!(grid[1][1], grid[2][1]);
    let cell_text = |id| {
        document
            .get_node(id)
            .expect("cell")
            .descendants()
            .filter(|node| node.has_tag_name((W, "p")))
            .map(paragraph_text)
            .collect::<String>()
    };
    assert_eq!(cell_text(grid[0][1]), "horizontal");
    assert_eq!(cell_text(grid[2][0]), "vertical");
    assert_eq!(cell_text(grid[2][1]), "lower");
}

#[test]
fn referenced_graphics_retain_vector_colour_but_do_not_bake_in_editable_text() {
    let mut fixture = PdfBuilder::new();
    fixture
        .page(200.0, 200.0)
        .text(20.0, 50.0, "EDITABLE")
        .rect([100.0, 100.0, 160.0, 140.0], Paint::fill([0.2, 0.7, 0.4]));
    let mut archive = convert(fixture);
    let part = document_part(&mut archive);
    let bytes = member(&mut archive, &part);
    let document =
        Xml::parse(std::str::from_utf8(&bytes).expect("document UTF-8")).expect("document XML");
    let text: String = document
        .descendants()
        .filter(|node| node.has_tag_name((W, "p")))
        .map(paragraph_text)
        .collect();
    assert_eq!(text, "EDITABLE");
    let blip = document
        .descendants()
        .find(|node| node.tag_name().name() == "blip")
        .expect("placed graphics image");
    let id = blip.attribute((R, "embed")).expect("image relationship");
    let path = std::path::Path::new(&part);
    let rels_name = path
        .parent()
        .expect("document folder")
        .join("_rels")
        .join(format!(
            "{}.rels",
            path.file_name().expect("document name").to_string_lossy()
        ));
    let rels_bytes = member(&mut archive, &rels_name.to_string_lossy());
    let rels = Xml::parse(std::str::from_utf8(&rels_bytes).expect("relationships UTF-8"))
        .expect("relationships XML");
    let target = rels
        .descendants()
        .find(|node| node.attribute("Id") == Some(id))
        .and_then(|node| node.attribute("Target"))
        .expect("image target");
    let image_path = path.parent().expect("document folder").join(target);
    let image =
        pdf_codec::decode_image_file(&member(&mut archive, &image_path.to_string_lossy()), 0)
            .expect("independent PNG decode");
    let rgba = image.to_rgba8();
    let offset =
        ((image.height as usize * 3 / 5) * image.width as usize + image.width as usize * 3 / 5) * 4;
    assert_eq!(&rgba[offset..offset + 4], &[51, 179, 102, 255]);
    for y in image.height as usize * 35 / 200..image.height as usize * 53 / 200 {
        for x in image.width as usize * 18 / 200..image.width as usize * 85 / 200 {
            assert_eq!(
                rgba[(y * image.width as usize + x) * 4 + 3],
                0,
                "editing the Word text must not reveal a baked-in original"
            );
        }
    }
}
