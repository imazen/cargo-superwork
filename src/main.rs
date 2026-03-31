#![forbid(unsafe_code)]

mod audit;
mod bump;
mod check;
mod ci;
mod config;
mod discover;
mod graph;
mod manifest;
mod patch;
mod publish;
mod readme_links;
mod run;
mod status;

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process;

#[derive(Parser)]
#[command(
    name = "cargo-superwork",
    version,
    about = "Multi-repo Rust workspace manager"
)]
struct Cli {
    /// Cargo passes "superwork" as the first arg when invoked as `cargo superwork`.
    #[arg(hide = true, default_value = "")]
    _cargo_subcommand: String,

    #[command(subcommand)]
    command: Command,

    /// Path to Superwork.toml (default: auto-detect)
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
        /// Only process this crate
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
    /// Check which crates need publishing (compares local vs crates.io)
    NeedsPublish {
        /// Show file-level diffs for changed crates
        #[arg(long)]
        show_diffs: bool,
        /// Only count src/ changes as real changes (ignore docs, CI, tests)
        #[arg(long)]
        src_only: bool,
    },
    /// Show per-repo git status, branch, push state, and version mismatches
    Status,
    /// Inventory all worktrees: dirty, unmerged, unpushed branches
    Worktrees,
    /// Generate "Image tech I maintain" ecosystem links section for READMEs
    ReadmeLinks {
        /// Crate to generate links for (highlights it in the table)
        #[arg(long)]
        crate_name: Option<String>,
    },
    /// Run health checks from work-maintenance (edition, license, badges, clutter, docs)
    Audit,

    // ── Cross-repo execution ──
    /// Run a shell command in every repo (dependency-ordered)
    Run {
        /// The command to run (e.g., "cargo test")
        cmd: String,
        /// Only repos matching this crate name glob
        #[arg(long)]
        filter: Option<String>,
        /// Only repos with changes since last git tag
        #[arg(long)]
        changed: bool,
        /// Max parallel jobs (default: 1)
        #[arg(long, short = 'j', default_value = "1")]
        jobs: usize,
        /// Stop on first failure
        #[arg(long)]
        fail_fast: bool,
    },
    /// Run `cargo test` across all repos (uses [checks.test] if defined)
    Test {
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        changed: bool,
        #[arg(long)]
        fail_fast: bool,
    },
    /// Run `cargo clippy` across all repos (uses [checks.clippy] if defined)
    Clippy {
        #[arg(long)]
        filter: Option<String>,
    },
    /// Check formatting across all repos
    Fmt {
        /// Fix formatting instead of checking
        #[arg(long)]
        fix: bool,
        #[arg(long)]
        filter: Option<String>,
    },
    /// Run cargo semver-checks on changed publishable crates
    SemverCheck {
        #[arg(long)]
        filter: Option<String>,
    },
    /// Test reverse dependencies of a crate using cargo-copter
    Copter {
        /// Crate to test reverse deps for
        name: String,
    },
    /// Check for outdated dependencies across all repos (requires cargo-outdated)
    Outdated {
        #[arg(long)]
        filter: Option<String>,
        /// Show all transitive deps, not just direct (depth=1)
        #[arg(long)]
        deep: bool,
    },
}

fn main() {
    let cli = Cli::parse();

    let config_path = cli.config.unwrap_or_else(|| {
        find_config().unwrap_or_else(|| {
            eprintln!("error: could not find Superwork.toml");
            eprintln!("  run from within the superworkspace, or pass --config <path>");
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

    let root = config_path.parent().unwrap().to_path_buf();

    let result = match cli.command {
        Command::Discover => discover::run(&root, &config),
        Command::Check => check::run(&root, &config),
        Command::CiPrep { crate_name } => {
            ci::run(&root, &config, crate_name.as_deref(), cli.dry_run)
        }
        Command::Patch => patch::run_patch(&root, &config, cli.dry_run),
        Command::Unpatch => patch::run_unpatch(&root, &config, cli.dry_run),
        Command::Bump { name, version } => bump::run(&root, &config, &name, &version, cli.dry_run),
        Command::PublishOrder => publish::run(&root, &config),
        Command::NeedsPublish {
            show_diffs,
            src_only,
        } => publish::run_needs_publish(&root, &config, show_diffs, src_only),
        Command::Status => status::run(&root, &config),
        Command::Worktrees => status::run_worktrees(&root, &config),
        Command::ReadmeLinks { crate_name } => {
            readme_links::run(&root, &config, crate_name.as_deref())
        }
        Command::Audit => audit::run(&root, &config),
        Command::Run {
            cmd,
            filter,
            changed,
            jobs,
            fail_fast,
        } => run::run_cmd(
            &root,
            &config,
            &cmd,
            filter.as_deref(),
            changed,
            jobs,
            fail_fast,
        ),
        Command::Test {
            filter,
            changed,
            fail_fast,
        } => run::run_check(
            &root,
            &config,
            "test",
            filter.as_deref(),
            changed,
            fail_fast,
        ),
        Command::Clippy { filter } => {
            run::run_check(&root, &config, "clippy", filter.as_deref(), false, true)
        }
        Command::Fmt { fix, filter } => {
            let cmd = if fix {
                "cargo fmt"
            } else {
                "cargo fmt -- --check"
            };
            run::run_cmd(&root, &config, cmd, filter.as_deref(), false, 1, true)
        }
        Command::SemverCheck { filter } => run::run_semver_check(&root, &config, filter.as_deref()),
        Command::Copter { name } => run::run_copter(&root, &config, &name),
        Command::Outdated { filter, deep } => {
            run::run_outdated(&root, &config, filter.as_deref(), deep)
        }
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn find_config() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        for name in ["Superwork.toml", "zen-ecosystem.toml"] {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}
