//! `capabilities` and `jobs`: the verbs that describe the tool and its history.

use clap::{Arg, ArgMatches, Command};
use goat_common::args::{int_value, optional, required};
use goat_common::ledger::recent_jobs;
use goat_common::{Ctx, GoatError, Registry, Verb};
use serde_json::{Map, Value};

use crate::registry::cli;

pub fn register(registry: &mut Registry) {
    registry.command(
        Verb::new(
            Command::new("capabilities")
                .about("discover command schemas for agents")
                .arg(
                    Arg::new("family")
                        .num_args(0..=1)
                        .help("top-level command family"),
                ),
            capabilities,
        )
        .unledgered(),
    );
    registry.command(
        Verb::new(
            Command::new("jobs").about("show the job ledger").arg(
                Arg::new("limit")
                    .long("limit")
                    .value_parser(int_value)
                    .default_value("20"),
            ),
            jobs,
        )
        .unledgered(),
    );
}

fn capabilities(matches: &ArgMatches, _ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let family = optional::<String>(matches, "family")?;
    cli().capabilities(family.map(String::as_str))
}

fn jobs(matches: &ArgMatches, ctx: &Ctx) -> Result<Map<String, Value>, GoatError> {
    let limit = *required::<i64>(matches, "limit")?;
    let jobs = recent_jobs(ctx.home(), limit)?;
    let mut result = Map::new();
    result.insert("verb".into(), "jobs".into());
    result.insert("inputs".into(), Value::Array(Vec::new()));
    result.insert("outputs".into(), Value::Array(Vec::new()));
    result.insert("count".into(), jobs.len().into());
    result.insert("jobs".into(), Value::Array(jobs));
    Ok(result)
}
