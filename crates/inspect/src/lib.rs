//! Inspection, metadata, attachment, security, repair, and size-reduction verbs of the
//! pdf-goat CLI, without qpdf or Ghostscript.
//!
//! Verbs: `info`, `inspect`, `preflight`, `meta get|set|strip`, `get
//! object|fonts|images|attachments`, `attach`, `detach`, `compare structure`,
//! `accessibility check|set`, `security encrypt|decrypt|permissions|sanitize`, `repair`,
//! `compress`, `optimize reduce`, and `convert pdfa`.
//!
//! `convert pdfa` creates a candidate, not a conformance certificate. It embeds missing
//! programs using installed TrueType substitutes while preserving text codes, extraction
//! mappings, and PDF advances. Existing embedded programs and Type 3 glyphs are retained.
//! Unsupported substitutes or ambiguous composite mappings return an error before the
//! destination is written. Locked inputs must be decrypted first; owner-only encryption
//! is removed. Every successful conversion reports `conformance_validated: false`.

use goat_common::Registry;

mod compare;
mod doc;
mod embed;
mod get;
mod meta;
mod optimize;
mod report;
mod security;

/// Registers every verb this crate provides.
pub fn register(registry: &mut Registry) {
    report::register(registry);
    meta::register(registry);
    get::register(registry);
    compare::register(registry);
    security::register(registry);
    optimize::register(registry);
}
