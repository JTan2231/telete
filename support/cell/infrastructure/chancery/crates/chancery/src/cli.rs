use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::model::{EntryKind, Mode, PromiseFacet};

/// Inspect installed local capability and operation contracts.
#[derive(Debug, Parser)]
#[command(
    name = "chancery",
    version,
    about = "Discover installed local capabilities and adaptive operations",
    long_about = "Read and validate installed, versioned provider contracts. Chancery never executes a documented interface, probes readiness, contacts a model, or changes provider state.",
    arg_required_else_help = true
)]
pub(crate) struct Cli {
    /// Provider registry directory. Overrides `CHANCERY_REGISTRY`.
    #[arg(
        long,
        global = true,
        env = "CHANCERY_REGISTRY",
        value_name = "ABSOLUTE_PATH"
    )]
    pub(crate) registry: Option<PathBuf>,

    /// Emit one versioned JSON document.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Register identities and inspect recorded command invocations.
    #[command(subcommand)]
    Usage(UsageCommand),
    /// List installed contracts.
    List(ListArgs),
    /// Read an installed product overview and its feature and operation inventory.
    Product {
        /// Exact installed provider ID.
        #[arg(value_name = "PROVIDER_ID")]
        provider: String,
    },
    /// Show one complete installed contract.
    Show(ShowArgs),
    /// Resolve one exact installed ID into its complete outward-promise dossier.
    Resolve(ResolveArgs),
    /// Validate the installed provider registry and dependencies.
    Doctor,
    /// Validate one standalone provider bundle.
    Validate(ValidateArgs),
}

#[derive(Debug, Subcommand)]
pub(crate) enum UsageCommand {
    /// Initialize the journal and register Chancery's commands.
    Init,
    /// Add a system and its command IDs. Existing identities remain unchanged.
    Register {
        system: String,
        #[arg(required = true)]
        commands: Vec<String>,
    },
    /// List registered systems.
    Systems,
    /// Count recorded uses for every registered command, including zero uses.
    Commands(UsageFilter),
    /// Read a bounded page of recorded invocations in insertion order.
    Events {
        #[command(flatten)]
        filter: UsageFilter,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
}

#[derive(Debug, Args)]
pub(crate) struct UsageFilter {
    #[arg(long)]
    pub(crate) system: Option<String>,
    #[arg(long, conflicts_with = "unattributed")]
    pub(crate) thread: Option<String>,
    #[arg(long)]
    pub(crate) unattributed: bool,
    /// Inclusive start, in Unix seconds.
    #[arg(long)]
    pub(crate) since: Option<i64>,
    /// Exclusive end, in Unix seconds.
    #[arg(long)]
    pub(crate) until: Option<i64>,
}

#[derive(Debug, Args)]
pub(crate) struct ListArgs {
    /// Restrict results to one exact installed provider ID.
    #[arg(long, value_name = "PROVIDER_ID")]
    pub(crate) provider: Option<String>,

    /// Restrict results to one work mode.
    #[arg(long, value_enum)]
    pub(crate) mode: Option<Mode>,

    /// Restrict results to capabilities or operations.
    #[arg(long, value_enum)]
    pub(crate) kind: Option<EntryKind>,
}

#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    /// Include structured authoring claims as well as the operating manual.
    #[arg(long)]
    pub(crate) full: bool,
    /// Stable capability or operation ID.
    #[arg(value_name = "ID")]
    pub(crate) id: String,
}

#[derive(Debug, Args)]
pub(crate) struct ResolveArgs {
    /// Return outcome, requirements and gaps without the complete contract dossiers.
    #[arg(long)]
    pub(crate) summary: bool,
    /// Exact stable capability or operation ID. Chancery does not match natural-language requests.
    #[arg(value_name = "ID")]
    pub(crate) id: String,

    /// Require the installed contract to be at least this version.
    #[arg(long, value_name = "VERSION")]
    pub(crate) min_contract: Option<u32>,

    /// Require the installed contract to be below this version.
    #[arg(long, value_name = "VERSION")]
    pub(crate) max_contract_exclusive: Option<u32>,

    /// Require a declared positive claim for this facet. May be repeated.
    #[arg(long, value_enum, value_name = "FACET")]
    pub(crate) require: Vec<PromiseFacet>,
}

#[derive(Debug, Args)]
pub(crate) struct ValidateArgs {
    /// Standalone provider bundle directory.
    #[arg(value_name = "BUNDLE")]
    pub(crate) bundle: PathBuf,
}

impl ValueEnum for Mode {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Use, Self::Operate, Self::Develop]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.as_str()))
    }
}

impl ValueEnum for EntryKind {
    fn value_variants<'a>() -> &'a [Self] {
        &[Self::Capability, Self::Operation]
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.as_str()))
    }
}

impl ValueEnum for PromiseFacet {
    fn value_variants<'a>() -> &'a [Self] {
        &Self::ALL
    }

    fn to_possible_value(&self) -> Option<clap::builder::PossibleValue> {
        Some(clap::builder::PossibleValue::new(self.as_str()))
    }
}
