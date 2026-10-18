use usher::api::{CheckReport, ErrorReport, inspect};

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    version,
    about = "Report declared Cell membership from repository evidence"
)]
struct Cli {
    #[arg(long, global = true, help = "Emit a schema-versioned JSON report")]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report every selected product, including incomplete introductions.
    Report(Selection),
    /// Report introductions and exit 1 if any selected product is incomplete.
    Check(Selection),
    /// Emit Iatreion's read-only operational observation.
    StatusSnapshot,
}

#[derive(Args)]
struct Selection {
    /// Cell checkout root. No installed service or Git history is consulted.
    #[arg(default_value = ".")]
    root: PathBuf,
    /// Select one descriptor ID or declared alias; collisions are still checked globally.
    #[arg(long)]
    product: Option<String>,
}

fn run(cli: Cli) -> Result<u8, String> {
    if matches!(cli.command, Command::StatusSnapshot) {
        let snapshot = iatreion_api::static_snapshot(
            "usher",
            env!("CARGO_PKG_VERSION"),
            vec![iatreion_api::on_demand_unit(
                "usher",
                "usher/report",
                "usher.recognition.inspect",
            )],
        );
        println!(
            "{}",
            serde_json::to_string(&snapshot).map_err(|error| error.to_string())?
        );
        return Ok(0);
    }
    let (selection, checking) = match cli.command {
        Command::Report(selection) => (selection, false),
        Command::Check(selection) => (selection, true),
        Command::StatusSnapshot => unreachable!(),
    };
    let report = inspect(&selection.root, selection.product.as_deref())?;
    let mut output = io::stdout().lock();
    if checking {
        let check = CheckReport::from(&report);
        if cli.json {
            serde_json::to_writer_pretty(&mut output, &check).map_err(|e| e.to_string())?;
            writeln!(output).map_err(|e| e.to_string())?;
        } else {
            check.render(&mut output).map_err(|e| e.to_string())?;
        }
    } else if cli.json {
        serde_json::to_writer_pretty(&mut output, &report).map_err(|e| e.to_string())?;
        writeln!(output).map_err(|e| e.to_string())?;
    } else {
        report.render(&mut output).map_err(|e| e.to_string())?;
    }
    Ok(u8::from(checking && report.incomplete > 0))
}

fn main() -> ExitCode {
    let cli = chancery_usage::cli::parse::<Cli>("usher", "");
    let json = cli.json;
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(message) => {
            if json {
                println!("{}", serde_json::json!(ErrorReport::new(message)));
            } else {
                eprintln!("usher: {message}");
            }
            ExitCode::from(2)
        }
    }
}
