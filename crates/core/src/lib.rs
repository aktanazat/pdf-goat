//! PDF object model, parser, writer, filters, page tree, and content streams.

mod content;
mod date;
mod document;
mod error;
pub mod filters;
mod geom;
mod lexer;
mod object;
mod pages;
mod parser;
mod serialize;
pub mod text;
mod trees;
mod writer;

pub use content::{Operation, parse_content, write_content};
pub use date::PdfDate;
pub use document::Document;
pub use error::{Error, Result};
pub use filters::{DecodedStream, StoppedFilter};
pub use geom::{Matrix, Point, Rect};
pub use object::{Dict, Name, ObjRef, Object, PdfString, Stream, StringFormat};
pub use pages::Page;
pub use parser::{parse_indirect_at, parse_object_at};
pub use pdf_crypt::{
    CryptMethod, DataKind, EncryptDict, NewEncryption, NewMethod, SecurityHandler,
};
pub use trees::{build_name_tree, build_number_tree};
pub use writer::{Encryption, IncrementalSave, SaveOptions, SignatureSpan};
