type TestResult = Result<(), Box<dyn std::error::Error>>;

#[derive(clap::Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    #[command(alias = "read")]
    Show { private_argument: String },
    #[command(subcommand)]
    Repository(Repository),
}

#[derive(clap::Subcommand)]
enum Repository {
    Show,
    List,
}

#[test]
fn command_identity_comes_from_declarations_not_argument_values() -> TestResult {
    use clap::CommandFactory as _;
    let ids = chancery_usage::cli::command_ids(&Cli::command(), "");
    assert!(ids.contains(&"repository.show".into()));
    assert!(ids.contains(&"show".into()));
    assert!(!ids.contains(&"read".into()));
    let (_, selected) = chancery_usage::cli::parse_command_from::<Cli>(
        ["example", "read", "private source text"].map(Into::into),
        "",
    )?;
    assert_eq!(selected.as_deref(), Some("show"));
    assert!(
        chancery_usage::cli::parse_command_from::<Cli>(["example", "show"].map(Into::into), "")
            .is_err()
    );
    Ok(())
}
