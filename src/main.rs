mod broker;
mod git;
mod invocation;
mod manager;
mod model;
mod paths;
mod process;
mod repository_queue;
mod runtime;
mod store;
mod workflow;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Write;
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
    /// Create a development branch and Git worktree.
    Worktree {
        path: PathBuf,
    },
    /// Submit an exact committed revision for background execution.
    Submit {
        #[arg(default_value = "HEAD")]
        revision: String,
        /// Observe the submission; interrupting observation leaves the job running.
        #[arg(long)]
        follow: bool,
    },
    Status {
        job: Option<String>,
    },
    /// Observe an existing job; Ctrl-C stops observation without cancelling work.
    Wait {
        job: String,
        /// Limit observation in seconds; the job continues after timeout.
        #[arg(long)]
        timeout: Option<u64>,
    },
    Pause,
    Resume,
    Cancel {
        job: String,
    },
    Recover {
        job: String,
    },
    Maintenance {
        #[command(subcommand)]
        command: Maintenance,
    },
    /// Inspect retained command gate records.
    Gates,
    #[command(hide = true)]
    InternalSupervise {
        #[arg(long)]
        state: PathBuf,
    },
    #[command(hide = true)]
    InternalWorker {
        #[arg(long)]
        state: PathBuf,
    },
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

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let cli = Cli::parse();
    let machine = cli.json;
    match execute(cli).await {
        Ok((value, code)) => {
            if let Err(error) = print_result(&value, machine) {
                eprintln!("telete: {error:#}");
                std::process::exit(1);
            }
            std::process::exit(code);
        }
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

fn print_result(value: &serde_json::Value, machine: bool) -> Result<()> {
    let stdout = std::io::stdout();
    let mut output = stdout.lock();
    if machine {
        serde_json::to_writer(&mut output, value)?;
    } else {
        serde_json::to_writer_pretty(&mut output, value)?;
    }
    writeln!(output)?;
    output.flush()?;
    Ok(())
}

async fn observe(
    paths: &paths::Paths,
    job: &str,
    timeout: Option<u64>,
) -> Result<(serde_json::Value, i32)> {
    let mut value = tokio::select! {
        result = manager::wait(paths, job, timeout) => result?,
        signal = tokio::signal::ctrl_c() => {
            signal.context("observe terminal interruption")?;
            std::process::exit(130);
        }
    };
    let code =
        i32::from(value["observation"] != "terminal" || value["job"]["phase"] != "succeeded");
    value["state_root"] = serde_json::to_value(&paths.root)?;
    Ok((value, code))
}

async fn execute(cli: Cli) -> Result<(serde_json::Value, i32)> {
    // Internal execution uses retained runtime identity, independent of the
    // submitting config and development worktree's subsequent lifetime.
    match &cli.command {
        Command::InternalExecute { state, directory } => {
            let paths = paths::Paths::selected(state.clone(), false)?;
            process::execute_request(&paths, directory)?;
            return Ok((serde_json::json!({"state":"completed"}), 0));
        }
        Command::InternalSupervise { state } => {
            let paths = paths::Paths::selected(state.clone(), false)?;
            runtime::supervise(&paths).await?;
            return Ok((serde_json::json!({"state":"stopped"}), 0));
        }
        Command::InternalWorker { state } => {
            let paths = paths::Paths::selected(state.clone(), false)?;
            manager::worker(&paths).await?;
            return Ok((serde_json::json!({"state":"stopped"}), 0));
        }
        _ => (),
    }
    let project = workflow::read(cli.config.as_deref().context("--config is required")?)?;
    if let Command::Worktree { path } = &cli.command {
        return Ok((repository_queue::create_worktree(&project, path)?, 0));
    }
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
    let follow = matches!(&cli.command, Command::Submit { follow: true, .. });
    let submission = matches!(&cli.command, Command::Submit { .. });
    let result = match cli.command {
        Command::Submit { revision, .. } => {
            manager::submit(&paths, &project.repo, &revision, project.workflow)
        }
        Command::Status { job } => manager::status(&paths, job.as_deref()),
        Command::Wait { job, timeout } => return observe(&paths, &job, timeout).await,
        Command::Pause => manager::pause(&paths),
        Command::Resume => manager::resume(&paths),
        Command::Cancel { job } => manager::cancel(&paths, &job),
        Command::Recover { job } => manager::recover(&paths, &job),
        Command::Maintenance { command } => match command {
            Maintenance::Hold { owner } => manager::maintenance_hold(&paths, &owner),
            Maintenance::Status => manager::maintenance_status(&paths),
            Maintenance::Release { owner } => manager::maintenance_release(&paths, &owner),
        },
        Command::Gates => broker::status(&paths),
        Command::InternalExecute { .. }
        | Command::InternalSupervise { .. }
        | Command::InternalWorker { .. }
        | Command::Worktree { .. } => unreachable!("handled before queue selection"),
    };
    // Even an unsuccessful mutation may have left admitted or cleanup work.
    let background = if create {
        runtime::ensure(&paths)
    } else {
        Ok(())
    };
    let mut value = result?;
    value["state_root"] = serde_json::to_value(&paths.root)?;
    if let Err(error) = background {
        if submission {
            value["worker_error"] = serde_json::Value::String(format!("{error:#}"));
            print_result(&value, cli.json)?;
            anyhow::bail!(
                "job {} was admitted, but background execution needs attention: {error:#}",
                value["id"]
            );
        }
        return Err(error);
    }
    if follow {
        print_result(&value, cli.json)?;
        return observe(
            &paths,
            value["id"].as_str().context("submitted job ID missing")?,
            None,
        )
        .await;
    }
    Ok((value, 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_defaults_to_head_and_uses_only_config_selection() {
        let cli = Cli::try_parse_from(["telete", "--config", "ci/workflow.json", "submit"])
            .expect("config and submit parse");
        assert!(
            matches!(cli.command, Command::Submit { revision, follow: false } if revision == "HEAD")
        );
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
