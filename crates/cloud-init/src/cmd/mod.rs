//! CLI definition and dispatch.

mod analyze;
mod clean;
mod collect_logs;
mod devel;
mod query;
mod schema;
mod stages;
mod status;

use std::path::{Path, PathBuf};

use ci_core::Paths;
use clap::{Parser, Subcommand};

/// Top-level `cloud-init` parser.
///
/// `--version` is defined manually because upstream binds it to `-v`, while
/// clap's built-in version flag uses `-V`.
#[derive(Debug, Parser)]
#[command(
    name = "cloud-init",
    about = "cloudinit: manage the initial configuration of a cloud instance",
    disable_version_flag = true,
    arg_required_else_help = true
)]
// Four flags, because upstream's top-level parser has four. Grouping them into
// a struct would change nothing a caller can see and would hide the mapping.
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Show program's version number and exit.
    #[arg(long, short = 'v')]
    pub version: bool,

    /// Show additional pre-action logging (default: false).
    #[arg(long, short = 'd')]
    pub debug: bool,

    /// Force running even if no datasource is found (use at your own risk).
    #[arg(long)]
    pub force: bool,

    /// Run cloud-init's stages under a single process using a synchronization
    /// protocol. This is not intended for CLI usage.
    #[arg(long = "all-stages")]
    pub all_stages: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Initialize cloud-init and perform initial modules.
    Init(stages::InitArgs),
    /// Activate modules using a given configuration key.
    Modules(stages::ModulesArgs),
    /// Run a single module.
    Single(stages::SingleArgs),
    /// List defined features.
    Features,
    /// Report cloud-init status or wait on completion.
    Status(status::Args),
    /// Query standardized instance metadata from the command line.
    Query(query::Args),
    /// Devel tool: Analyze cloud-init logs and data.
    Analyze(analyze::Args),
    /// Collect and tar all cloud-init debug info.
    #[command(name = "collect-logs")]
    CollectLogs(collect_logs::Args),
    /// Run development tools.
    Devel(devel::Args),
    /// Remove logs and artifacts so cloud-init can re-run.
    Clean(clean::Args),
    /// Validate cloud-config files using jsonschema.
    Schema(schema::Args),
}

/// Run the parsed command, returning the process exit code.
pub fn dispatch(cli: &Cli) -> u8 {
    if cli.version {
        // argparse renders `%(prog)s`, which upstream leaves as argv[0].
        let prog = std::env::args()
            .next()
            .unwrap_or_else(|| "cloud-init".to_owned());
        println!("{prog} {}", ci_core::version::version_string());
        return 0;
    }
    // `--all-stages` runs the boot itself and ignores whatever subcommand was
    // also given, exactly as upstream's `if not args.all_stages` does.
    if cli.all_stages {
        return stages::run_all_stages();
    }
    match &cli.command {
        Some(Command::Init(args)) => stages::run_init(args, cli.force),
        Some(Command::Modules(args)) => stages::run_modules(args, cli.force),
        Some(Command::Single(args)) => stages::run_single(args, cli.force),
        Some(Command::Features) => {
            print!("{}", ci_core::features::render());
            0
        }
        Some(Command::Status(args)) => status::run(args),
        Some(Command::Query(args)) => query::run(args),
        Some(Command::Analyze(args)) => analyze::run(args),
        Some(Command::CollectLogs(args)) => collect_logs::run(args),
        Some(Command::Devel(args)) => devel::run(args),
        Some(Command::Clean(args)) => clean::run(args),
        Some(Command::Schema(args)) => schema::run(args),
        None => {
            eprintln!("cloud-init: error: a subcommand is required");
            2
        }
    }
}

/// Pick the instance-data file to read.
///
/// Root prefers the sensitive copy, which contains unredacted metadata; if it is
/// absent we fall back to the world-readable file and say so, exactly as upstream
/// does. Non-root callers never get the sensitive path, so a wrong-permissions
/// deployment cannot leak secrets through the CLI.
pub fn instance_data_path(paths: &Paths, requested: Option<&Path>) -> PathBuf {
    if let Some(path) = requested {
        return path.to_path_buf();
    }
    let redacted = paths.instance_data_file();
    if !ci_sys::is_root() {
        return redacted;
    }
    let sensitive = paths.instance_data_sensitive_file();
    if sensitive.exists() {
        return sensitive;
    }
    eprintln!(
        "Missing root-readable {}. Using redacted {} instead.",
        sensitive.display(),
        redacted.display()
    );
    redacted
}
