mod broker;
mod git;
mod invocation;
mod manager;
mod model;
mod paths;
mod process;
mod store;
mod workflow;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "telete",
    version,
    about = "Committed-source CI for repository workflows"
)]
struct Cli {
    /// Project JSON configuration. Relative paths are allowed.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run an exact committed revision using the selected project configuration.
    Submit {
        #[arg(default_value = "HEAD")]
        revision: String,
    },
    Status {
        job: Option<String>,
    },
    Wait {
        job: String,
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
    Pause,
    Resume,
    Cancel {
        job: String,
    },
    Recover {
        job: String,
    },
    /// Run the foreground queue worker.
    Worker {
        #[arg(long)]
        once: bool,
    },
    Maintenance {
        #[command(subcommand)]
        command: Maintenance,
    },
    /// Inspect retained command gate records.
    Gates,
    #[command(hide = true)]
    InternalExecute {
        #[arg(long)]
        state: PathBuf,
        #[arg(long)]
        directory: PathBuf,
    },
}

#[derive(Subcommand)]
enum Maintenance {
    Hold {
        #[arg(long)]
        owner: String,
    },
    Status,
    Release {
        #[arg(long)]
        owner: String,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let machine = cli.json;
    let result = execute(cli).await.and_then(|value| {
        if machine {
            Ok(value.to_string())
        } else {
            Ok(serde_json::to_string_pretty(&value)?)
        }
    });
    match result {
        Ok(output) => println!("{output}"),
        Err(error) => {
            if machine {
                eprintln!(
                    "{}",
                    serde_json::json!({"ok":false,"error":format!("{error:#}")})
                );
            } else {
                eprintln!("telete: {error:#}");
            }
            std::process::exit(1);
        }
    }
}

async fn execute(cli: Cli) -> Result<serde_json::Value> {
    if let Command::InternalExecute { state, directory } = &cli.command {
        let paths = paths::Paths::selected(state.clone(), false)?;
        process::execute_request(&paths, directory)?;
        return Ok(serde_json::json!({"state":"completed"}));
    }
    let project = workflow::read(cli.config.as_deref().context("--config is required")?)?;
    let create = !matches!(
        cli.command,
        Command::Status { .. }
            | Command::Wait { .. }
            | Command::Gates
            | Command::Maintenance {
                command: Maintenance::Status
            }
    );
    let paths = manager::select(&project, create)?;
    match cli.command {
        Command::Submit { revision } => {
            manager::submit(&paths, &project.repo, &revision, project.workflow)
        }
        Command::Status { job } => manager::status(&paths, job.as_deref()),
        Command::Wait { job, timeout } => manager::wait(&paths, &job, timeout).await,
        Command::Pause => manager::pause(&paths),
        Command::Resume => manager::resume(&paths),
        Command::Cancel { job } => manager::cancel(&paths, &job),
        Command::Recover { job } => manager::recover(&paths, &job),
        Command::Worker { once } => {
            if once {
                manager::worker_once(&paths)
            } else {
                manager::worker(&paths).await?;
                Ok(serde_json::json!({"state":"stopped"}))
            }
        }
        Command::Maintenance { command } => match command {
            Maintenance::Hold { owner } => manager::maintenance_hold(&paths, &owner),
            Maintenance::Status => manager::maintenance_status(&paths),
            Maintenance::Release { owner } => manager::maintenance_release(&paths, &owner),
        },
        Command::Gates => broker::status(&paths),
        Command::InternalExecute { .. } => unreachable!("handled before config selection"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_defaults_to_head_and_uses_only_config_selection() {
        let cli = Cli::try_parse_from(["telete", "--config", "ci/workflow.json", "submit"])
            .expect("config and submit parse");
        assert!(matches!(cli.command, Command::Submit { revision } if revision == "HEAD"));
        for args in [
            vec!["telete", "--state", "/tmp/state", "submit"],
            vec!["telete", "--config", "telete.json", "init"],
            vec!["telete", "--config", "telete.json", "submit", "--repo", "."],
            vec![
                "telete",
                "--config",
                "telete.json",
                "submit",
                "--workflow",
                "workflow.json",
            ],
            vec![
                "telete",
                "--config",
                "telete.json",
                "submit",
                "--request-id",
                "key",
            ],
        ] {
            assert!(Cli::try_parse_from(args).is_err());
        }
    }

    #[test]
    fn supervisor_can_execute_without_config() {
        let cli = Cli::try_parse_from([
            "telete",
            "internal-execute",
            "--state",
            "/tmp/state",
            "--directory",
            "/tmp/request",
        ])
        .expect("internal execution needs only its own arguments");
        assert!(cli.config.is_none());
        assert!(matches!(cli.command, Command::InternalExecute { .. }));
    }
}
