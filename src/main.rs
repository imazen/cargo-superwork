#![forbid(unsafe_code)]

mod bump;
mod check;
mod ci;
mod config;
mod discover;
mod graph;
mod manifest;
mod patch;
mod publish;
mod status;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process;

#[derive(Parser)]
#[command(
    name = "cargo-zen",
    version,
    about = "Ecosystem dependency manager for multi-repo Rust workspaces"
)]
struct Cli {
    /// Cargo passes "zen" as the first arg when invoked as `cargo zen`.
    /// Accept and ignore it.
    #[arg(hide = true, default_value = "")]
    _cargo_subcommand: String,

    #[command(subcommand)]
    command: Command,

    /// Path to zen-ecosystem.toml (default: auto-detect)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Show what would change without writing files
    #[arg(long, global = true)]
    dry_run: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Scan repos and show discovered crates and dependencies
    Discover,
    /// Validate ecosystem health: dual-spec, version consistency, path validity
    Check,
    /// Prepare Cargo.toml files for CI (replace paths with git URLs or versions)
    CiPrep {
        /// Only process this crate (default: all crates with CI overrides)
        #[arg(long)]
        crate_name: Option<String>,
    },
    /// Add path overrides to all internal deps (dev mode)
    Patch,
    /// Remove path overrides from dual-specified deps (publish mode)
    Unpatch,
    /// Bump a crate version and update all dependents
    Bump {
        /// Crate name to bump
        name: String,
        /// New version
        version: String,
    },
    /// Show topological publish order
    PublishOrder,
    /// Show per-repo git status and version mismatches
    Status,
}

fn main() {
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(|| {
        // Auto-detect: look for zen-ecosystem.toml in CWD, then parent dirs
        find_config().unwrap_or_else(|| {
            eprintln!("error: could not find zen-ecosystem.toml");
            eprintln!("  run from within the ecosystem root, or pass --config <path>");
            process::exit(1);
        })
    });

    let config = match config::load_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: failed to load config: {e}");
            process::exit(1);
        }
    };

    let ecosystem_root = config_path.parent().unwrap().to_path_buf();

    let result = match cli.command {
        Command::Discover => discover::run(&ecosystem_root, &config),
        Command::Check => check::run(&ecosystem_root, &config),
        Command::CiPrep { crate_name } => {
            ci::run(&ecosystem_root, &config, crate_name.as_deref(), cli.dry_run)
        }
        Command::Patch => patch::run_patch(&ecosystem_root, &config, cli.dry_run),
        Command::Unpatch => patch::run_unpatch(&ecosystem_root, &config, cli.dry_run),
        Command::Bump { name, version } => {
            bump::run(&ecosystem_root, &config, &name, &version, cli.dry_run)
        }
        Command::PublishOrder => publish::run(&ecosystem_root, &config),
        Command::Status => status::run(&ecosystem_root, &config),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn find_config() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let candidate = dir.join("zen-ecosystem.toml");
        if candidate.exists() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}
