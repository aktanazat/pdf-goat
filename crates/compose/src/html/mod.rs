//! HTML/CSS print layout with bounded local/network assets and no browser.

mod assets;
mod boxes;
mod css;
mod dom;
mod fonts;
mod format;
mod format_layout;
mod image;
mod layout;
mod pdf;
mod position;
mod shaping;
mod style;
mod svg;
mod tables;

use goat_common::GoatError;
use std::path::Path;

pub fn render(html: &str, base: &Path) -> Result<Vec<u8>, GoatError> {
    if html.len() > 32 * 1024 * 1024 {
        return Err(GoatError::message("HTML input exceeds the 32 MiB limit"));
    }
    let dom = dom::Dom::parse(html);
    let mut base = url::Url::from_directory_path(base)
        .map_err(|()| GoatError::message("invalid document base directory"))?;
    if let Some(href) = (0..dom.nodes.len())
        .filter(|&id| dom.tag(id) == "base")
        .find_map(|id| dom.attr(id, "href"))
        && let Ok(resolved) = base.join(href)
    {
        base = resolved;
    }
    let assets = assets::Assets::new();
    let mut fonts = fonts::Fonts::new();
    let mut images = image::Images::default();
    let mut stylist = style::Stylist::new(&dom, &assets, &base);
    let page = stylist.page();
    let (block, background) =
        boxes::build(&dom, &mut stylist, &mut fonts, &mut images, &assets, &base);
    let canvas = layout::layout(&block, &mut fonts, &images, page, background)?;
    let title = dom
        .root_element()
        .and_then(|root| dom.child_named(root, "head"))
        .and_then(|head| dom.child_named(head, "title"))
        .map_or_else(String::new, |title| dom.text_content(title));
    pdf::write(canvas, &fonts, images, page, &title)
}
