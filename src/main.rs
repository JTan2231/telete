mod broker;
mod git;
mod invocation;
mod manager;
mod model;
mod paths;
mod process;
mod store;
mod workflow;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "telete",
    version,
    about = "Committed-source CI for repository workflows"
)]
struct Cli {
    /// Absolute private directory for the queue and its working files.
    #[arg(long)]
    state: PathBuf,
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a paused queue, optionally with an accepted-source baseline.
    Init {
        #[arg(long)]
        repo: PathBuf,
        #[arg(long)]
        accepted_baseline: Option<String>,
    },
    /// Freeze committed source and workflow commands for a new job.
    Submit {
        commit: String,
        #[arg(long, default_value = ".")]
        repo: PathBuf,
        #[arg(long)]
        request_id: Option<String>,
        /// Freeze commands and parameters from an absolute JSON file.
        #[arg(long)]
        workflow: PathBuf,
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
    let paths = paths::Paths::selected(cli.state, matches!(cli.command, Command::Init { .. }))?;
    match cli.command {
        Command::Init {
            repo,
            accepted_baseline,
        } => manager::init(&paths, &repo, accepted_baseline.as_deref()),
        Command::Submit {
            commit,
            repo,
            request_id,
            workflow,
        } => manager::submit(
            &paths,
            &repo,
            &commit,
            manager::SubmitOptions {
                request_id,
                workflow: workflow::read(&workflow)?,
            },
        ),
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
        Command::InternalExecute { directory } => {
            process::execute_request(&paths, &directory)?;
            Ok(serde_json::json!({"state":"completed"}))
        }
    }
}
