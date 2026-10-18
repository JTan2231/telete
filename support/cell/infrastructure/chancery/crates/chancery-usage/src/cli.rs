//! Adapters for existing Clap dispatchers. Only declared command names are
//! recorded; argument values, aliases, and external subcommand text are not IDs.

use crate::{Result, Store, default_path};
use clap::{ArgMatches, Command, Parser};
use std::ffi::OsString;

fn joined(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}

/// Return the complete declared inventory, including currently unused commands.
pub fn command_ids(command: &Command, prefix: &str) -> Vec<String> {
    let mut result = Vec::new();
    let children: Vec<_> = command
        .get_subcommands()
        .filter(|c| c.get_name() != "help")
        .collect();
    if !prefix.is_empty() && (children.is_empty() || !command.is_subcommand_required_set()) {
        result.push(prefix.to_owned());
    }
    for child in children {
        result.extend(command_ids(child, &joined(prefix, child.get_name())));
    }
    result.sort();
    result.dedup();
    result
}

fn selected(command: &Command, matches: &ArgMatches, prefix: &str) -> Option<String> {
    match matches.subcommand() {
        Some(("help", _)) => None,
        Some((name, child_matches)) => command
            .find_subcommand(name)
            .and_then(|child| selected(child, child_matches, &joined(prefix, child.get_name()))),
        None => (!prefix.is_empty()).then(|| prefix.to_owned()),
    }
}

/// Parse without recording, for products that perform further command parsing.
pub fn parse_command_from<T: Parser>(
    arguments: impl IntoIterator<Item = OsString>,
    prefix: &str,
) -> std::result::Result<(T, Option<String>), clap::Error> {
    let mut command = T::command();
    let matches = command.try_get_matches_from_mut(arguments)?;
    let parsed = T::from_arg_matches(&matches)?;
    Ok((parsed, selected(&command, &matches, prefix)))
}

/// Explicit post-install registration mode. It runs no product command.
pub fn registration_requested() -> bool {
    std::env::args_os()
        .skip(1)
        .eq([OsString::from("--register-usage")])
}

pub fn register_ids(system: &str, ids: &[String]) -> Result<()> {
    let store = Store::initialize(&default_path()?)?;
    store.register_system(system)?;
    for id in ids {
        store.register_command(system, id)?;
    }
    Ok(())
}

pub fn registration_exit(system: &str, ids: &[String]) -> ! {
    match register_ids(system, ids) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("chancery usage: {error}");
            std::process::exit(1)
        }
    }
}

pub fn register_if_requested<T: Parser>(system: &str, prefix: &str) {
    if registration_requested() {
        let mut ids = command_ids(&T::command(), prefix);
        if prefix.is_empty() {
            ids.push("status-snapshot".to_owned());
        }
        registration_exit(system, &ids);
    }
}

/// Preserve Clap's error handling and record exactly once after a valid parse.
pub fn try_parse<T: Parser>(system: &str, prefix: &str) -> std::result::Result<T, clap::Error> {
    register_if_requested::<T>(system, prefix);
    let (parsed, command) = parse_command_from::<T>(std::env::args_os(), prefix)?;
    if let Some(command) = command {
        crate::observe(system, &command);
    }
    Ok(parsed)
}

pub fn parse<T: Parser>(system: &str, prefix: &str) -> T {
    try_parse(system, prefix).unwrap_or_else(|error| error.exit())
}
