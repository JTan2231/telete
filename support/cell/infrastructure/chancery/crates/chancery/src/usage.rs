use chancery_usage::{Filter, Store};
use clap::CommandFactory as _;
use serde_json::json;

use crate::cli::{Cli, UsageCommand, UsageFilter};
use crate::error::AppError;
use crate::render::CommandOutput;

impl From<&UsageFilter> for Filter {
    fn from(value: &UsageFilter) -> Self {
        Self {
            system: value.system.clone(),
            thread: value.thread.clone(),
            unattributed: value.unattributed,
            since: value.since,
            until: value.until,
        }
    }
}

pub(crate) fn run(command: &UsageCommand) -> Result<CommandOutput, AppError> {
    execute(command).map_err(|error| AppError::invalid("usage_failed", error.to_string()))
}

fn execute(command: &UsageCommand) -> chancery_usage::Result<CommandOutput> {
    let path = chancery_usage::default_path()?;
    let output = match command {
        UsageCommand::Init => {
            let mut ids = chancery_usage::cli::command_ids(&Cli::command(), "");
            ids.push("status-snapshot".to_owned());
            chancery_usage::cli::register_ids("chancery", &ids)?;
            json!({"initialized":true,"schema_version":1})
        }
        UsageCommand::Register { system, commands } => {
            let store = Store::open(&path)?;
            store.register_system(system)?;
            for command in commands {
                store.register_command(system, command)?;
            }
            json!({"system_id":system,"commands":commands})
        }
        UsageCommand::Systems => json!({"items": Store::read(&path)?.systems()?}),
        UsageCommand::Commands(filter) => {
            let filter = Filter::from(filter);
            json!({"scope":filter,"items":Store::read(&path)?.counts(&filter)?})
        }
        UsageCommand::Events {
            filter,
            after,
            limit,
        } => {
            let filter = Filter::from(filter);
            json!({"scope":filter,"page":Store::read(&path)?.events(&filter,*after,*limit)?})
        }
    };
    let human = serde_json::to_string_pretty(&output)
        .map_err(|_| chancery_usage::Error::Invalid("cannot encode usage report"))?;
    Ok(CommandOutput::success(output, human))
}
