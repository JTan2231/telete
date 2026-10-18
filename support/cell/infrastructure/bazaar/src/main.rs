use anyhow::{Context, Result, ensure};
use bazaar::api::{Reader, Writer};
use clap::{Args, Parser, Subcommand};
use serde_json::json;
use std::io::Read;
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "Store and retrieve immutable versions of text")]
struct Cli {
    /// Select an absolute database path. Defaults to ~/.local/share/bazaar/bazaar.sqlite3.
    #[arg(long, global = true)]
    database: Option<PathBuf>,
    /// Emit schema-one JSON instead of readable text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize empty state or check the existing database schema.
    Init,
    /// Check database integrity without changing state.
    Doctor,
    /// Read the latest version, or one exact version.
    Get {
        id: String,
        #[arg(long)]
        version: Option<i64>,
    },
    /// List all version numbers for one ID, newest first.
    History { id: String },
    /// Append the exact supplied content as a new version.
    Update {
        id: String,
        #[command(flatten)]
        input: ContentInput,
    },
    /// Import reviewed prompt components and publish exact owner selections.
    ImportPrompts { file: PathBuf },
}

#[derive(Args)]
#[group(id = "content_source", required = true, multiple = false)]
struct ContentInput {
    /// Complete replacement content. Empty strings are valid.
    content: Option<String>,
    /// Read exact UTF-8 content from a file.
    #[arg(long)]
    file: Option<PathBuf>,
    /// Read exact UTF-8 content from standard input.
    #[arg(long)]
    stdin: bool,
}

fn main() {
    let cli = chancery_usage::cli::parse::<Cli>("bazaar", "");
    let json_output = cli.json;
    if let Err(error) = run(cli) {
        if json_output {
            println!(
                "{}",
                json!({"schema_version":1,"ok":false,"error":{"detail":format!("{error:#}")}})
            );
        } else {
            eprintln!("bazaar: {error:#}");
        }
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    let database = if let Some(path) = cli.database {
        path
    } else {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unavailable")?);
        ensure!(home.is_absolute(), "HOME must be absolute");
        bazaar::database_path(&home)
    };
    let data = match &cli.command {
        Command::Init => {
            Writer::initialize(&database)?;
            json!({"schema_version":1})
        }
        Command::Doctor => {
            Reader::open(&database)?.check()?;
            json!({"schema_version":1,"integrity":"ok"})
        }
        Command::Get { id, version } => {
            serde_json::to_value(Reader::open(&database)?.get(id, *version)?)?
        }
        Command::History { id } => {
            let versions = Reader::open(&database)?.history(id)?;
            json!({"id":id,"versions":versions})
        }
        Command::Update { id, input } => {
            let content = if let Some(content) = &input.content {
                content.clone()
            } else if let Some(path) = &input.file {
                std::fs::read_to_string(path).context("cannot read UTF-8 content file")?
            } else {
                let mut content = String::new();
                std::io::stdin()
                    .read_to_string(&mut content)
                    .context("cannot read UTF-8 content from standard input")?;
                content
            };
            serde_json::to_value(Writer::open(&database)?.update(id, &content)?)?
        }
        Command::ImportPrompts { file } => {
            serde_json::to_value(bazaar::prompts::import(&database, file)?)?
        }
    };
    if cli.json {
        println!("{}", json!({"schema_version":1,"ok":true,"data":data}));
    } else {
        match cli.command {
            Command::Init => println!("Initialized Bazaar database: {}", database.display()),
            Command::Doctor => println!(
                "Bazaar database integrity: ok\nDatabase: {}",
                database.display()
            ),
            Command::Get { id, .. } => {
                println!("ID: {id}\nVersion: {}\n\nContent:", data["version"]);
                let content = data["content"].as_str().context("missing string content")?;
                print!("{content}");
                if !content.ends_with('\n') {
                    println!();
                }
            }
            Command::History { id } => {
                println!("Versions for {id} (newest first):");
                if let Some(versions) = data["versions"].as_array() {
                    for version in versions {
                        println!("  {version}");
                    }
                }
            }
            Command::Update { id, .. } => println!("Stored {id} version {}.", data["version"]),
            Command::ImportPrompts { .. } => println!(
                "Imported {} prompt IDs; appended {} text versions; published {} selections.",
                data["imported_ids"], data["appended_text_versions"], data["owner_selections"]
            ),
        }
    }
    Ok(())
}
