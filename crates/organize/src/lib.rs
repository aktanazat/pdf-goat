//! Page organization, document outlines, and links for the pdf-goat CLI.
//!
//! [`register`] adds 27 standalone commands. The page-copying commands preserve page
//! content and remap internal references, but do not import a source catalog's outline.
//! In-place deletion disables outline destinations to deleted pages and removes links
//! to them. Reordering keeps surviving page references and cleans invalid destinations.
//!
//! Page layout uses PDF Form XObjects rather than rasterizing. Header and numbering
//! text use Helvetica; Unicode watermarks use the shared TrueType subset writer.
//! Every command returns the documented JSON result through `goat-common`.

mod bookmarks;
mod dest;
mod geometry;
mod links;
mod open;
mod outline;
mod pages;
mod select;
mod show;
mod stamp;
mod structure;
mod watermark_text;

use goat_common::Registry;

/// Registers every verb this crate provides.
pub fn register(registry: &mut Registry) {
    structure::register(registry);
    pages::register_before_layout(registry);
    show::register(registry);
    stamp::register(registry);
    pages::register_after_layout(registry);
    bookmarks::register(registry);
    links::register(registry);
}
