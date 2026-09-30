//! Shared plumbing for every pdf-goat verb: the command registry and argparse-compatible
//! parser, the error type, the Python-exact argument helpers, the page pool, the text cache,
//! and the job ledger.

pub mod args;
pub mod command;
pub mod ctx;
pub mod error;
pub mod ledger;
pub mod output;
pub mod parse;
pub mod paths;
pub mod pool;
pub mod py;
pub mod textcache;

pub use command::{Cli, Handler, Invocation, ParseFailure, Registry, Verb};
pub use ctx::Ctx;
pub use error::GoatError;
