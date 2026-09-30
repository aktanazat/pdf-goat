//! argparse value types for clap arguments, and typed reads of parsed arguments.

use std::any::Any;
use std::ffi::OsStr;

use clap::builder::{PossibleValue, TypedValueParser};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{Arg, ArgMatches, Command};

use crate::error::GoatError;
use crate::py::{PyInt, parse_float, parse_int};

/// argparse `type=int`: Python `int()` text, so surrounding whitespace, `_` between digits,
/// and any Unicode decimal digits parse. Values past `i64` saturate.
pub fn int_value(text: &str) -> Result<i64, GoatError> {
    Ok(match parse_int(text)? {
        PyInt::Small(value) => value,
        PyInt::Huge(digits) if digits.starts_with('-') => i64::MIN,
        PyInt::Huge(_) => i64::MAX,
    })
}

/// argparse `type=float`: Python `float()` text.
pub fn float_value(text: &str) -> Result<f64, GoatError> {
    parse_float(text)
}

/// argparse `type=int, choices=(...)`: parsed as `int()` parses, then checked against the
/// choices, which `capabilities` lists as integers.
#[derive(Clone, Copy, Debug)]
pub struct IntChoices(pub &'static [&'static str]);

impl TypedValueParser for IntChoices {
    type Value = i64;

    fn parse_ref(
        &self,
        cmd: &Command,
        arg: Option<&Arg>,
        value: &OsStr,
    ) -> Result<i64, clap::Error> {
        let parsed = value.to_str().and_then(|text| int_value(text).ok());
        let allowed = parsed.filter(|number| {
            self.0
                .iter()
                .any(|choice| int_value(choice).is_ok_and(|choice| choice == *number))
        });
        allowed.ok_or_else(|| {
            let mut error = clap::Error::new(ErrorKind::InvalidValue).with_cmd(cmd);
            if let Some(arg) = arg {
                error.insert(
                    ContextKind::InvalidArg,
                    ContextValue::String(arg.to_string()),
                );
            }
            error.insert(
                ContextKind::InvalidValue,
                ContextValue::String(value.to_string_lossy().into_owned()),
            );
            error
        })
    }

    fn possible_values(&self) -> Option<Box<dyn Iterator<Item = PossibleValue> + '_>> {
        Some(Box::new(
            self.0.iter().map(|choice| PossibleValue::new(*choice)),
        ))
    }
}

/// An argument's value, or `None` when the command line left it unset. Fails only when
/// `id` or `T` disagrees with the argument's definition.
pub fn optional<'m, T: Any + Clone + Send + Sync + 'static>(
    matches: &'m ArgMatches,
    id: &str,
) -> Result<Option<&'m T>, GoatError> {
    matches
        .try_get_one::<T>(id)
        .map_err(|error| GoatError::exception("TypeError", error.to_string()))
}

/// The value of an argument clap always fills: required, or with a default.
pub fn required<'m, T: Any + Clone + Send + Sync + 'static>(
    matches: &'m ArgMatches,
    id: &str,
) -> Result<&'m T, GoatError> {
    optional(matches, id)?
        .ok_or_else(|| GoatError::exception("TypeError", format!("argument {id} has no value")))
}

/// Every value of a repeatable or multi-value argument; empty when absent.
pub fn many<'m, T: Any + Clone + Send + Sync + 'static>(
    matches: &'m ArgMatches,
    id: &str,
) -> Result<Vec<&'m T>, GoatError> {
    let values = matches
        .try_get_many::<T>(id)
        .map_err(|error| GoatError::exception("TypeError", error.to_string()))?;
    Ok(values.map(Iterator::collect).unwrap_or_default())
}

/// A `store_true` flag.
pub fn flag(matches: &ArgMatches, id: &str) -> Result<bool, GoatError> {
    required::<bool>(matches, id).copied()
}
