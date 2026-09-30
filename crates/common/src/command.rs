//! The command registry: verbs register a clap command and a handler; the binary composes
//! them into one parser whose errors read like the Python parser's and whose shape
//! `capabilities` describes.

use std::any::TypeId;
use std::ffi::OsString;

use clap::builder::ValueRange;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::parser::ValueSource;
use clap::{Arg, ArgAction, ArgGroup, ArgMatches, Command};
use serde_json::{Map, Number, Value};

use crate::ctx::Ctx;
use crate::error::GoatError;

/// What a verb returns: the result object, whose `verb`, `inputs`, and `outputs` keys the
/// binary reads for the ledger and output.
pub type Handler = fn(&ArgMatches, &Ctx) -> Result<Map<String, Value>, GoatError>;

/// One runnable command: a clap command, its handler, and how the binary treats it.
#[derive(Clone, Debug)]
pub struct Verb {
    command: Command,
    traits: Traits,
}

/// Everything about a verb except its parser.
#[derive(Clone, Debug)]
struct Traits {
    handler: Handler,
    ledger: bool,
    renamed: Vec<(String, String)>,
    without_default: Vec<String>,
}

impl Traits {
    /// The name `capabilities` reports for argument `id`.
    fn schema_id<'a>(&'a self, id: &'a str) -> &'a str {
        self.renamed
            .iter()
            .find(|(from, _)| from == id)
            .map_or(id, |(_, to)| to.as_str())
    }
}

impl Verb {
    /// A ledgered verb. Argument ids must equal the Python `dest` names.
    pub fn new(command: Command, handler: Handler) -> Self {
        Self {
            command,
            traits: Traits {
                handler,
                ledger: true,
                renamed: Vec::new(),
                without_default: Vec::new(),
            },
        }
    }

    /// The same verb, never written to the job ledger.
    pub fn unledgered(mut self) -> Self {
        self.traits.ledger = false;
        self
    }

    /// Reports argument `id` as `name` in `capabilities`, for two flags that share one
    /// Python `dest` while clap needs distinct ids.
    pub fn schema_name(mut self, id: &str, name: &str) -> Self {
        self.traits.renamed.push((id.to_owned(), name.to_owned()));
        self
    }

    /// Omits argument `id`'s default from `capabilities`, for a Python flag whose default is
    /// `None` where clap reports one.
    pub fn schema_without_default(mut self, id: &str) -> Self {
        self.traits.without_default.push(id.to_owned());
        self
    }

    /// The command name.
    pub fn name(&self) -> &str {
        self.command.get_name()
    }
}

/// A registered top-level entry.
#[derive(Debug)]
enum Top {
    Command(Box<Verb>),
    Family(&'static str, Vec<Verb>),
}

impl Top {
    fn name(&self) -> &str {
        match self {
            Self::Command(verb) => verb.name(),
            Self::Family(name, _) => name,
        }
    }
}

/// Every command the binary offers, in registration order.
#[derive(Debug, Default)]
pub struct Registry {
    tops: Vec<Top>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a top-level command such as `jobs` or `text`.
    pub fn command(&mut self, verb: Verb) {
        self.tops.push(Top::Command(Box::new(verb)));
    }

    /// Registers `verb` under the top-level family `family`, such as `annotate highlight`.
    /// A family lists its verbs in registration order.
    pub fn family_verb(&mut self, family: &'static str, verb: Verb) {
        let existing = self.tops.iter_mut().find_map(|top| match top {
            Top::Family(name, verbs) if *name == family => Some(verbs),
            _ => None,
        });
        match existing {
            Some(verbs) => verbs.push(verb),
            None => self.tops.push(Top::Family(family, vec![verb])),
        }
    }

    /// Composes the parser under `root` (name, about, and root flags). Top-level names come
    /// in `order`, then any others in registration order; `family_help` gives each family's
    /// about text.
    pub fn into_cli(
        self,
        root: Command,
        order: &[&str],
        family_help: &[(&str, &'static str)],
    ) -> Cli {
        let mut tops = self.tops;
        tops.sort_by_key(|top| {
            order
                .iter()
                .position(|name| *name == top.name())
                .unwrap_or(order.len())
        });
        let mut root = root
            .subcommand_required(true)
            .disable_help_subcommand(true)
            .disable_version_flag(true)
            .infer_long_args(true)
            .args_override_self(true);
        let mut leaves = Vec::new();
        for top in tops {
            match top {
                Top::Command(verb) => {
                    let verb = *verb;
                    leaves.push(Leaf {
                        family: None,
                        name: verb.name().to_owned(),
                        traits: verb.traits,
                    });
                    root = root.subcommand(verb.command);
                }
                Top::Family(name, verbs) => {
                    let mut family = Command::new(name).subcommand_required(true);
                    if let Some((_, help)) = family_help.iter().find(|(family, _)| *family == name)
                    {
                        family = family.about(*help);
                    }
                    for verb in verbs {
                        leaves.push(Leaf {
                            family: Some(name),
                            name: verb.name().to_owned(),
                            traits: verb.traits,
                        });
                        family = family.subcommand(verb.command);
                    }
                    root = root.subcommand(family);
                }
            }
        }
        let root = allow_negative_numbers(root);
        let mut built = root.clone();
        built.build();
        Cli {
            root,
            built,
            leaves,
        }
    }
}

/// A runnable command in the composed parser.
#[derive(Debug)]
struct Leaf {
    family: Option<&'static str>,
    name: String,
    traits: Traits,
}

/// argparse reads `-5` as a value when no option looks like a number; clap needs it asked.
fn allow_negative_numbers(command: Command) -> Command {
    command
        .mut_args(|arg| {
            let takes_values = arg.get_action().takes_values()
                && arg.get_num_args().is_none_or(|range| range.takes_values());
            if takes_values {
                arg.allow_negative_numbers(true)
            } else {
                arg
            }
        })
        .mut_subcommands(allow_negative_numbers)
}

/// The composed parser.
#[derive(Debug)]
pub struct Cli {
    root: Command,
    built: Command,
    leaves: Vec<Leaf>,
}

/// Why a command line produced no verb to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseFailure {
    /// `-h` or `--help`: print this text to stdout and exit 0.
    Help(String),
    /// A usage error, worded as the Python parser words it.
    Usage(String),
}

/// A parsed command line, ready to run.
#[derive(Debug)]
pub struct Invocation<'c> {
    /// The top-level command name; errors report it as their verb.
    pub name: String,
    /// The leaf command's arguments.
    pub matches: ArgMatches,
    traits: &'c Traits,
}

impl Invocation<'_> {
    /// Whether the run goes in the job ledger.
    pub fn ledgered(&self) -> bool {
        self.traits.ledger
    }

    /// Runs the handler.
    pub fn run(&self, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
        (self.traits.handler)(&self.matches, ctx)
    }
}

impl Cli {
    /// Parses `args` (without the program name).
    ///
    /// Unknown arguments are reported only when nothing else is wrong, as argparse reports
    /// them after its other checks.
    pub fn parse(&self, args: &[OsString]) -> Result<Invocation<'_>, ParseFailure> {
        let mut args = args.to_vec();
        let mut unrecognized = false;
        loop {
            let error = match self.root.clone().try_get_matches_from(with_program(&args)) {
                Ok(matches) if !unrecognized => return self.invocation(matches),
                Ok(_) => return Err(usage("unrecognized arguments")),
                Err(error) => error,
            };
            match error.kind() {
                ErrorKind::DisplayHelp => {
                    return Err(ParseFailure::Help(error.render().to_string()));
                }
                ErrorKind::UnknownArgument => {
                    let token = context_string(&error, ContextKind::InvalidArg);
                    if let Some(token) = token
                        && self.is_ambiguous(&args, token)
                    {
                        return Err(usage("ambiguous option"));
                    }
                    let position = token.and_then(|token| {
                        args.iter().position(|arg| {
                            let arg = arg.to_string_lossy();
                            arg == token
                                || arg
                                    .strip_prefix(token)
                                    .is_some_and(|rest| rest.starts_with('='))
                        })
                    });
                    let Some(position) = position else {
                        return Err(usage("unrecognized arguments"));
                    };
                    args.remove(position);
                    unrecognized = true;
                }
                _ => return Err(usage(&self.message(&args, &error))),
            }
        }
    }

    fn invocation(&self, mut matches: ArgMatches) -> Result<Invocation<'_>, ParseFailure> {
        let Some((name, mut top)) = matches.remove_subcommand() else {
            return Err(usage("the following arguments are required: cmd"));
        };
        let (leaf, found) = match top.remove_subcommand() {
            Some((verb, leaf)) => (leaf, self.find_leaf(Some(&name), &verb)),
            None => (top, self.find_leaf(None, &name)),
        };
        let Some(found) = found else {
            return Err(usage("argument cmd"));
        };
        Ok(Invocation {
            name,
            matches: leaf,
            traits: &found.traits,
        })
    }

    fn find_leaf(&self, family: Option<&str>, name: &str) -> Option<&Leaf> {
        self.leaves
            .iter()
            .find(|leaf| leaf.family == family && leaf.name == name)
    }

    /// Whether `token` is a `--` prefix of more than one option where the parse stopped.
    fn is_ambiguous(&self, args: &[OsString], token: &str) -> bool {
        let Some(prefix) = token.strip_prefix("--") else {
            return false;
        };
        let command = command_path(&self.built, args).pop().unwrap_or(&self.built);
        command
            .get_arguments()
            .filter_map(Arg::get_long)
            .filter(|long| long.starts_with(prefix))
            .count()
            > 1
    }

    /// The Python parser's message for a clap error other than help or unknown arguments.
    fn message(&self, args: &[OsString], error: &clap::Error) -> String {
        // Clap prints an argument only once its command is built.
        let path = command_path(&self.built, args);
        let command = path.last().copied().unwrap_or(&self.built);
        let named = |kind| {
            context_string(error, kind)
                .and_then(|shown| find_arg(command, shown))
                .map(argparse_name)
        };
        match error.kind() {
            ErrorKind::InvalidSubcommand => {
                format!(
                    "argument {}",
                    subcommand_dest(command.get_name(), path.len() > 1)
                )
            }
            ErrorKind::MissingSubcommand => format!(
                "the following arguments are required: {}",
                subcommand_dest(command.get_name(), path.len() > 1)
            ),
            ErrorKind::MissingRequiredArgument => self
                .missing_message(args)
                .unwrap_or_else(|| kind_text(error.kind())),
            ErrorKind::ArgumentConflict => {
                let earlier = named(ContextKind::InvalidArg);
                let later = match error.get(ContextKind::PriorArg) {
                    Some(ContextValue::String(shown)) => {
                        find_arg(command, shown).map(argparse_name)
                    }
                    Some(ContextValue::Strings(shown)) => shown
                        .first()
                        .and_then(|shown| find_arg(command, shown))
                        .map(argparse_name),
                    _ => None,
                };
                match (later, earlier) {
                    (Some(later), Some(earlier)) => {
                        format!("argument {later}: not allowed with argument {earlier}")
                    }
                    _ => kind_text(error.kind()),
                }
            }
            ErrorKind::InvalidValue => {
                let Some(name) = named(ContextKind::InvalidArg) else {
                    return kind_text(error.kind());
                };
                let missing = matches!(error.get(ContextKind::InvalidValue), Some(ContextValue::String(value)) if value.is_empty())
                    && !args
                        .iter()
                        .any(|arg| arg.is_empty() || arg.to_string_lossy().ends_with('='));
                if missing {
                    let at_least = context_string(error, ContextKind::InvalidArg)
                        .and_then(|shown| find_arg(command, shown))
                        .and_then(Arg::get_num_args)
                        .is_some_and(|range| range.max_values() > 1);
                    let expected = if at_least {
                        "at least one argument"
                    } else {
                        "one argument"
                    };
                    format!("argument {name}: expected {expected}")
                } else {
                    format!("argument {name}")
                }
            }
            ErrorKind::TooFewValues => named(ContextKind::InvalidArg).map_or_else(
                || kind_text(error.kind()),
                |name| format!("argument {name}: expected at least one argument"),
            ),
            ErrorKind::ValueValidation | ErrorKind::WrongNumberOfValues | ErrorKind::NoEquals => {
                named(ContextKind::InvalidArg).map_or_else(
                    || kind_text(error.kind()),
                    |name| format!("argument {name}"),
                )
            }
            ErrorKind::TooManyValues => "unrecognized arguments".to_owned(),
            other => kind_text(other),
        }
    }

    /// argparse's missing-argument message: every required argument absent from the
    /// command line, or else the first required group with no member present.
    fn missing_message(&self, args: &[OsString]) -> Option<String> {
        let relaxed = relax(self.root.clone());
        let mut matches = &relaxed.try_get_matches_from(with_program(args)).ok()?;
        let mut command = &self.root;
        while let Some((name, sub)) = matches.subcommand() {
            command = command.find_subcommand(name)?;
            matches = sub;
        }
        let present = |id: &str| matches!(matches.value_source(id), Some(ValueSource::CommandLine));
        let missing: Vec<String> = command
            .get_arguments()
            .filter(|arg| arg.is_required_set() && !present(arg.get_id().as_str()))
            .map(argparse_name)
            .collect();
        if !missing.is_empty() {
            return Some(format!(
                "the following arguments are required: {}",
                missing.join(", ")
            ));
        }
        let group = command.get_groups().find(|group| {
            group.is_required_set() && !group.get_args().any(|id| present(id.as_str()))
        })?;
        let names: Vec<String> = group
            .get_args()
            .filter_map(|id| command.get_arguments().find(|arg| arg.get_id() == id))
            .map(argparse_name)
            .collect();
        Some(format!(
            "one of the arguments {} is required",
            names.join(" ")
        ))
    }

    /// `pdf-goat capabilities [family]`: the command tree as JSON schemas.
    pub fn capabilities(&self, requested: Option<&str>) -> Result<Map<String, Value>, GoatError> {
        let tops: Vec<&Command> = self.built.get_subcommands().collect();
        let mut families: Vec<&str> = Vec::new();
        let mut commands: Vec<&str> = Vec::new();
        for top in &tops {
            if top.get_subcommands().next().is_some() {
                families.push(top.get_name());
            } else {
                commands.push(top.get_name());
            }
        }
        families.sort_unstable();
        commands.sort_unstable();
        let mut schemas = Map::new();
        if let Some(name) = requested.filter(|name| !name.is_empty()) {
            let top = tops
                .iter()
                .find(|top| top.get_name() == name)
                .ok_or_else(|| GoatError::message(format!("unknown top-level command: {name}")))?;
            schemas.insert(name.to_owned(), self.command_schema(top, &[name]));
        }
        let root_arguments: Vec<Value> = schema_arguments(&self.built)
            .map(|arg| Value::Object(argument_schema(arg, None)))
            .collect();
        let mut result = Map::new();
        result.insert("verb".into(), "capabilities".into());
        result.insert("inputs".into(), Value::Array(Vec::new()));
        result.insert("outputs".into(), Value::Array(Vec::new()));
        result.insert("schema_version".into(), 1.into());
        result.insert("mode".into(), "standalone".into());
        result.insert("agent_json".into(), true.into());
        result.insert("root_arguments".into(), Value::Array(root_arguments));
        result.insert("families".into(), families.into());
        result.insert("commands".into(), commands.into());
        result.insert(
            "requested_command".into(),
            requested.map_or(Value::Null, Value::from),
        );
        result.insert("command_count".into(), leaf_count(&self.built).into());
        result.insert("schemas".into(), Value::Object(schemas));
        Ok(result)
    }

    fn command_schema(&self, command: &Command, words: &[&str]) -> Value {
        let traits = match words {
            [name] => self.find_leaf(None, name),
            [family, name] => self.find_leaf(Some(family), name),
            _ => None,
        }
        .map(|leaf| &leaf.traits);
        let arguments: Vec<Value> = schema_arguments(command)
            .map(|arg| Value::Object(argument_schema(arg, traits)))
            .collect();
        let commands: Map<String, Value> = command
            .get_subcommands()
            .map(|sub| {
                let mut path = words.to_vec();
                path.push(sub.get_name());
                (sub.get_name().to_owned(), self.command_schema(sub, &path))
            })
            .collect();
        let mut schema = Map::new();
        schema.insert(
            "command".into(),
            format!("pdf-goat {}", words.join(" ")).into(),
        );
        schema.insert("arguments".into(), Value::Array(arguments));
        schema.insert("commands".into(), Value::Object(commands));
        let groups: Vec<Value> = command
            .get_groups()
            .filter(|group| !ArgGroup::clone(group).is_multiple())
            .map(|group| {
                let members: Vec<Value> = group
                    .get_args()
                    .map(|id| {
                        Value::from(
                            traits.map_or(id.as_str(), |traits| traits.schema_id(id.as_str())),
                        )
                    })
                    .collect();
                let mut entry = Map::new();
                entry.insert("required".into(), group.is_required_set().into());
                entry.insert("arguments".into(), Value::Array(members));
                Value::Object(entry)
            })
            .collect();
        if !groups.is_empty() {
            schema.insert("mutually_exclusive_groups".into(), Value::Array(groups));
        }
        Value::Object(schema)
    }
}

fn usage(message: &str) -> ParseFailure {
    ParseFailure::Usage(message.to_owned())
}

fn with_program(args: &[OsString]) -> impl Iterator<Item = OsString> + '_ {
    std::iter::once(OsString::from("pdf-goat")).chain(args.iter().cloned())
}

fn context_string(error: &clap::Error, kind: ContextKind) -> Option<&str> {
    match error.get(kind) {
        Some(ContextValue::String(value)) => Some(value),
        _ => None,
    }
}

fn kind_text(kind: ErrorKind) -> String {
    kind.as_str().unwrap_or("invalid command line").to_owned()
}

/// The Python `dest` of a subparsers action: `cmd` at the root, `{family}_cmd` below it.
fn subcommand_dest(name: &str, nested: bool) -> String {
    if nested {
        format!("{name}_cmd")
    } else {
        "cmd".to_owned()
    }
}

/// The commands the argument walk descends through: each token that names a subcommand of
/// the current command, skipping options.
fn command_path<'a>(root: &'a Command, args: &[OsString]) -> Vec<&'a Command> {
    let mut path = vec![root];
    let mut current = root;
    for arg in args {
        if current.get_subcommands().next().is_none() {
            break;
        }
        let token = arg.to_string_lossy();
        if token.starts_with('-') {
            continue;
        }
        let Some(sub) = current.find_subcommand(token.as_ref()) else {
            break;
        };
        path.push(sub);
        current = sub;
    }
    path
}

/// The argument clap printed as `shown` in an error.
fn find_arg<'a>(command: &'a Command, shown: &str) -> Option<&'a Arg> {
    command.get_arguments().find(|arg| arg.to_string() == shown)
}

/// argparse's `_get_action_name`: an option's flags joined by `/`, else the positional's
/// metavar, else its dest.
fn argparse_name(arg: &Arg) -> String {
    if arg.is_positional() {
        return arg
            .get_value_names()
            .and_then(<[_]>::first)
            .map_or_else(|| arg.get_id().to_string(), ToString::to_string);
    }
    flags(arg).join("/")
}

fn flags(arg: &Arg) -> Vec<String> {
    let short = arg.get_short().map(|short| format!("-{short}"));
    let long = arg.get_long().map(|long| format!("--{long}"));
    short.into_iter().chain(long).collect()
}

/// Clears every requirement so a re-parse reports what the command line holds.
fn relax(command: Command) -> Command {
    let groups: Vec<String> = command
        .get_groups()
        .map(|group| group.get_id().to_string())
        .collect();
    let mut command = command
        .subcommand_required(false)
        .mut_args(|arg| arg.required(false));
    for id in groups {
        command = command.mut_group(id, |group| group.required(false));
    }
    command.mut_subcommands(relax)
}

fn leaf_count(command: &Command) -> usize {
    command
        .get_subcommands()
        .map(|sub| {
            if sub.get_subcommands().next().is_some() {
                leaf_count(sub)
            } else {
                1
            }
        })
        .sum()
}

/// A command's arguments without clap's own help and version flags.
fn schema_arguments(command: &Command) -> impl Iterator<Item = &Arg> {
    command.get_arguments().filter(|arg| {
        !matches!(
            arg.get_action(),
            ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
        )
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Boolean,
    Integer,
    Number,
    Text,
}

impl ValueKind {
    fn of(arg: &Arg) -> Self {
        if !arg.get_num_args().is_some_and(|range| range.takes_values()) {
            return Self::Boolean;
        }
        let parsed = arg.get_value_parser().type_id();
        let integers = [
            TypeId::of::<i64>(),
            TypeId::of::<i32>(),
            TypeId::of::<i16>(),
            TypeId::of::<i8>(),
            TypeId::of::<u64>(),
            TypeId::of::<u32>(),
            TypeId::of::<u16>(),
            TypeId::of::<u8>(),
            TypeId::of::<usize>(),
            TypeId::of::<isize>(),
        ];
        if integers.into_iter().any(|id| parsed == id) {
            Self::Integer
        } else if parsed == TypeId::of::<f64>() || parsed == TypeId::of::<f32>() {
            Self::Number
        } else {
            Self::Text
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Boolean => "boolean",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Text => "string",
        }
    }

    /// A clap value string as the JSON value Python would have held.
    fn value(self, text: &str) -> Value {
        match self {
            Self::Boolean => Value::Bool(text == "true"),
            Self::Integer => text
                .parse::<i64>()
                .map_or_else(|_| Value::from(text), Value::from),
            Self::Number => match text.parse::<i64>() {
                Ok(integer) => Value::from(integer),
                Err(_) => text
                    .parse::<f64>()
                    .ok()
                    .and_then(Number::from_f64)
                    .map_or_else(|| Value::from(text), Value::Number),
            },
            Self::Text => Value::from(text),
        }
    }
}

/// argparse's `nargs` for a value range: omitted for exactly one value.
fn nargs(range: ValueRange) -> Option<Value> {
    match (range.min_values(), range.max_values()) {
        (1, 1) => None,
        (0, 1) => Some("?".into()),
        (1, usize::MAX) => Some("+".into()),
        (0, usize::MAX) => Some("*".into()),
        (min, max) if min == max => Some(min.into()),
        _ => None,
    }
}

/// One argument in `capabilities` order: name, flags, required, type, repeatable, nargs,
/// choices, default, help.
fn argument_schema(arg: &Arg, traits: Option<&Traits>) -> Map<String, Value> {
    let id = arg.get_id().as_str();
    let kind = ValueKind::of(arg);
    let mut schema = Map::new();
    schema.insert(
        "name".into(),
        traits.map_or(id, |traits| traits.schema_id(id)).into(),
    );
    schema.insert("flags".into(), flags(arg).into());
    schema.insert("required".into(), arg.is_required_set().into());
    schema.insert("type".into(), kind.as_str().into());
    if !arg.is_positional() && matches!(arg.get_action(), ArgAction::Append) {
        schema.insert("repeatable".into(), true.into());
    }
    if let Some(nargs) = arg.get_num_args().and_then(nargs) {
        schema.insert("nargs".into(), nargs);
    }
    if kind != ValueKind::Boolean {
        let choices: Vec<Value> = arg
            .get_possible_values()
            .iter()
            .filter(|choice| !choice.is_hide_set())
            .map(|choice| kind.value(choice.get_name()))
            .collect();
        if !choices.is_empty() {
            schema.insert("choices".into(), Value::Array(choices));
        }
    }
    let omit_default =
        traits.is_some_and(|traits| traits.without_default.iter().any(|omitted| omitted == id));
    let defaults: Vec<Value> = arg
        .get_default_values()
        .iter()
        .map(|value| kind.value(&value.to_string_lossy()))
        .collect();
    if !omit_default {
        match <[Value; 1]>::try_from(defaults) {
            Ok([default]) => {
                schema.insert("default".into(), default);
            }
            Err(defaults) if !defaults.is_empty() => {
                schema.insert("default".into(), Value::Array(defaults));
            }
            Err(_) => {}
        }
    }
    if let Some(help) = arg.get_help() {
        schema.insert("help".into(), help.to_string().into());
    }
    schema
}
