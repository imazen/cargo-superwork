#![forbid(unsafe_code)]

mod audit;
mod bump;
mod check;
mod ci;
mod ci_gen;
mod ci_lint;
mod config;
mod dashboard;
mod discover;
mod fix_dual_spec;
mod graph;
mod manifest;
mod patch;
mod publish;
mod readme_links;
mod release;
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
    /// Unified dashboard: branch, CI, dirty, unpushed in one table
    Dashboard,
    /// Generate "Image tech I maintain" ecosystem links section for READMEs
    ReadmeLinks {
        /// Crate to generate links for (highlights it in the table)
        #[arg(long)]
        crate_name: Option<String>,
    },
    /// Run health checks from work-maintenance (edition, license, badges, clutter, docs)
    Audit,
    /// Lint CI readiness: detect deps that will break after ci-prep transformation
    CiLint {
        /// Only check repos matching this crate name glob
        #[arg(long)]
        filter: Option<String>,
        /// Query crates.io to verify published versions (slower but more accurate)
        #[arg(long)]
        online: bool,
        /// Show passing deps too (default: only warn/error/info)
        #[arg(long)]
        verbose: bool,
    },

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
    /// Release orchestration: init, analyze, categorize, check, publish
    Release {
        #[command(subcommand)]
        command: release::ReleaseCommand,
    },
    /// Generate/sync CI workflow files across all repos from a template
    CiGen {
        /// Path to template file (default: ci-template.yml in workspace root)
        #[arg(long)]
        template: Option<String>,
        /// Only repos matching this crate name glob
        #[arg(long)]
        filter: Option<String>,
    },
    /// Clone all ecosystem repos on a new machine
    Setup {
        /// Actually clone (default: dry-run showing what would be cloned)
        #[arg(long)]
        run: bool,
        /// Use SSH URLs (git@github.com:) instead of HTTPS
        #[arg(long)]
        ssh: bool,
    },
    /// Add version specs to path-only internal deps (makes them dual-specified for publish)
    FixDualSpec {
        /// Only fix deps from crates matching this glob
        #[arg(long)]
        filter: Option<String>,
        /// Only fix deps targeting this specific crate
        #[arg(long)]
        target: Option<String>,
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
        Command::Dashboard => dashboard::run(&root, &config),
        Command::ReadmeLinks { crate_name } => {
            readme_links::run(&root, &config, crate_name.as_deref())
        }
        Command::Audit => audit::run(&root, &config),
        Command::CiLint {
            filter,
            online,
            verbose,
        } => ci_lint::run(&root, &config, filter.as_deref(), online, verbose),
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
        Command::CiGen { template, filter } => ci_gen::run(
            &root,
            &config,
            template.as_deref(),
            filter.as_deref(),
            cli.dry_run,
        ),
        Command::Release { command } => release::run(&root, &config, &command, cli.dry_run),
        Command::Setup { run, ssh } => run_setup(&root, &config, run, ssh),
        Command::FixDualSpec { filter, target } => fix_dual_spec::run(
            &root,
            &config,
            filter.as_deref(),
            target.as_deref(),
            cli.dry_run,
        ),
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}

fn run_setup(
    root: &std::path::Path,
    config: &config::SuperworkConfig,
    execute: bool,
    ssh: bool,
) -> Result<(), String> {
    // Build repo list from config (not from scan — repos may not exist yet)
    let mut repos: Vec<(String, String)> = Vec::new();
    let meta = config.meta();

    // Collect repo dirs from [[repo]] overrides
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for r in &config.repo {
        if r.no_remote {
            continue;
        }
        let gh = r.github.as_deref().unwrap_or_else(|| {
            std::path::Path::new(&r.dir)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(&r.dir)
        });
        let url = format_clone_url(gh, &meta.default_github_org, ssh);
        repos.push((r.dir.clone(), url));
        seen.insert(r.dir.clone());
    }

    // Scan existing repos to find any not covered by [[repo]] overrides
    // (best-effort: some dirs may not exist yet on a fresh machine)
    if let Ok(eco) = discover::scan_ecosystem(root, config) {
        for info in eco.crates.values() {
            if !seen.contains(&info.repo_dir) {
                if let Some(gh_url) = &info.github_url {
                    let slug = gh_url.strip_prefix("https://github.com/").unwrap_or(gh_url);
                    let url = if ssh {
                        format!("git@github.com:{slug}.git")
                    } else {
                        format!("https://github.com/{slug}")
                    };
                    repos.push((info.repo_dir.clone(), url));
                    seen.insert(info.repo_dir.clone());
                }
            }
        }
    }

    repos.sort_by(|a, b| a.0.cmp(&b.0));

    let label = if execute { "" } else { "[dry-run] " };
    println!(
        "{label}Setting up {} repos for '{}' superworkspace",
        repos.len(),
        meta.name.as_deref().unwrap_or("default")
    );
    println!();

    let mut cloned = 0;
    let mut existed = 0;
    let mut failed = 0;

    for (dir, url) in &repos {
        let target = root.join(dir);

        if target.join(".git").exists() || target.join(".git").is_file() {
            println!("{label}  exists: {dir}");
            existed += 1;
            continue;
        }

        println!("{label}  clone:  {dir} ← {url}");

        if !execute {
            cloned += 1;
            continue;
        }

        // Ensure parent directory exists
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }

        let status = std::process::Command::new("git")
            .args(["clone", url, &target.to_string_lossy()])
            .status()
            .map_err(|e| format!("git clone {url}: {e}"))?;

        if status.success() {
            cloned += 1;
        } else {
            eprintln!("  ERROR: git clone failed for {dir}");
            failed += 1;
        }
    }

    println!();
    println!(
        "{label}{cloned} cloned, {existed} already existed, {failed} failed (of {} total)",
        repos.len()
    );

    if !execute && cloned > 0 {
        println!();
        println!("Run with --run to actually clone. Add --ssh for SSH URLs.");
    }

    Ok(())
}

fn format_clone_url(slug_or_name: &str, default_org: &str, ssh: bool) -> String {
    let slug = if slug_or_name.contains('/') {
        slug_or_name.to_string()
    } else {
        format!("{default_org}/{slug_or_name}")
    };
    if ssh {
        format!("git@github.com:{slug}.git")
    } else {
        format!("https://github.com/{slug}")
    }
}

fn find_config() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        // Check for Superwork.toml directly
        for name in ["Superwork.toml", "zen-ecosystem.toml"] {
            let candidate = dir.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
        // Check for pointer file (.superwork-root contains relative path to Superwork.toml)
        let pointer = dir.join(".superwork-root");
        if pointer.exists() {
            if let Ok(content) = std::fs::read_to_string(&pointer) {
                let target = dir.join(content.trim());
                if target.exists() {
                    return Some(target);
                }
            }
        }
        if !dir.pop() {
            return None;
        }
    }
}
