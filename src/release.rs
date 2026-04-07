//! Release orchestration subcommand group.
//!
//! Manages the lifecycle of a coordinated release across the ecosystem:
//! init → analyze → categorize → check → local-test → ci-status → publish

use crate::config::{CrateClass, SuperworkConfig};
use crate::discover::{self, CrateInfo, Ecosystem};
use crate::graph;
use clap::Subcommand;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

// ── CLI ──

#[derive(Subcommand)]
pub enum ReleaseCommand {
    /// Drive a full release wave: bump → commit → push → tag → CI → publish, tier by tier
    Wave {
        /// Advance the wave by one step (default: show current state)
        #[arg(long)]
        advance: bool,
    },
    /// Scan ecosystem and generate per-repo release analysis files
    Init {
        /// Regenerate even if analysis files already exist
        #[arg(long)]
        force: bool,
    },
    /// Show uncategorized crates for AI to review and categorize
    Analyze {
        /// Only show crates in this tier
        #[arg(long)]
        tier: Option<usize>,
        /// Only show uncategorized crates
        #[arg(long)]
        uncategorized: bool,
    },
    /// Write AI categorization for a crate
    Categorize {
        /// Crate name
        name: String,
        /// Change category: breaking, feature, fix, perf, docs, deps, internal, skip
        #[arg(long)]
        category: String,
        /// Version bump level: major, minor, patch, skip
        #[arg(long)]
        bump: String,
        /// Reason for the categorization
        #[arg(long)]
        reason: Option<String>,
    },
    /// Run cargo semver-checks (and optionally copter) for a tier
    Check {
        /// Tier to check
        #[arg(long)]
        tier: Option<usize>,
        /// Also run cargo-copter for crates with dependents
        #[arg(long)]
        copter: bool,
    },
    /// Run local tests: native, cross targets, clippy, fmt
    LocalTest {
        /// Tier to test
        #[arg(long)]
        tier: Option<usize>,
        /// Specific cross target (e.g., i686-unknown-linux-gnu)
        #[arg(long)]
        target: Option<String>,
        /// Only run clippy + fmt (skip cargo test)
        #[arg(long)]
        lint_only: bool,
    },
    /// Apply version bumps from categorization decisions
    Bump {
        /// Tier to bump
        #[arg(long)]
        tier: Option<usize>,
    },
    /// Create git tags and GitHub releases for a tier
    Tag {
        /// Tier to tag
        tier: usize,
    },
    /// Check CI status for a tier, applying allowlist
    CiStatus {
        /// Tier to check
        #[arg(long)]
        tier: Option<usize>,
    },
    /// Publish crates for a tier (requires tags + CI passed)
    Publish {
        /// Tier to publish
        tier: usize,
    },
    /// Show full release status table
    Status,
    /// Suggest what AI should do next
    Next,
}

pub fn run(
    root: &Path,
    config: &SuperworkConfig,
    command: &ReleaseCommand,
    dry_run: bool,
) -> Result<(), String> {
    match command {
        ReleaseCommand::Wave { advance } => run_wave(root, config, *advance, dry_run),
        ReleaseCommand::Init { force } => run_init(root, config, *force, dry_run),
        ReleaseCommand::Analyze {
            tier,
            uncategorized,
        } => run_analyze(root, config, *tier, *uncategorized),
        ReleaseCommand::Categorize {
            name,
            category,
            bump,
            reason,
        } => run_categorize(root, config, name, category, bump, reason.as_deref()),
        ReleaseCommand::Check { tier, copter } => run_check(root, config, *tier, *copter),
        ReleaseCommand::LocalTest {
            tier,
            target,
            lint_only,
        } => run_local_test(root, config, *tier, target.as_deref(), *lint_only),
        ReleaseCommand::Bump { tier } => run_bump(root, config, *tier, dry_run),
        ReleaseCommand::Tag { tier } => run_tag(root, config, *tier, dry_run),
        ReleaseCommand::CiStatus { tier } => run_ci_status(root, config, *tier),
        ReleaseCommand::Publish { tier } => run_publish(root, config, *tier, dry_run),
        ReleaseCommand::Status => run_status(root, config),
        ReleaseCommand::Next => run_next(root, config),
    }
}

// ── Per-repo analysis file schema ──

const ANALYSIS_FILENAME: &str = "release-analysis.toml";
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct RepoAnalysis {
    pub schema_version: u32,
    #[serde(default)]
    pub superworkspace: String,
    #[serde(default)]
    pub generated_at: String,
    #[serde(default)]
    pub crates: BTreeMap<String, CrateAnalysis>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CrateAnalysis {
    // Git analysis (tool-generated)
    #[serde(default)]
    pub head: String,
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub tag_found: bool,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub class: String,
    #[serde(default)]
    pub tier: Option<usize>,
    #[serde(default)]
    pub dependents: usize,
    #[serde(default)]
    pub files_changed: usize,
    #[serde(default)]
    pub lines_added: usize,
    #[serde(default)]
    pub lines_removed: usize,
    #[serde(default)]
    pub src_files_changed: usize,
    #[serde(default)]
    pub commit_count: usize,
    #[serde(default)]
    pub commit_subjects: Vec<String>,

    // AI categorization (AI fills these)
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub bump: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub needs_review: bool,

    // Check results (tool fills)
    #[serde(default)]
    pub semver_check: String,
    #[serde(default)]
    pub copter: String,
}

// ── Wave state (persisted in workspace repo) ──

const WAVE_FILENAME: &str = "release-wave.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TierStatus {
    Pending,
    Bumped,
    Pushed,
    Tagged,
    CiWatching,
    CiPassed,
    Publishing,
    Published,
}

impl std::fmt::Display for TierStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Bumped => write!(f, "bumped"),
            Self::Pushed => write!(f, "pushed"),
            Self::Tagged => write!(f, "tagged"),
            Self::CiWatching => write!(f, "ci_watching"),
            Self::CiPassed => write!(f, "ci_passed"),
            Self::Publishing => write!(f, "publishing"),
            Self::Published => write!(f, "published"),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct WaveState {
    #[serde(default)]
    pub started_at: String,
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub tiers: BTreeMap<String, TierState>,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TierState {
    pub status: String,
    #[serde(default)]
    pub crates: Vec<String>,
    /// GitHub Actions run IDs per repo (repo_dir → run_id)
    #[serde(default)]
    pub ci_runs: BTreeMap<String, String>,
    /// CI conclusions per repo
    #[serde(default)]
    pub ci_results: BTreeMap<String, String>,
}

pub fn load_wave(root: &Path) -> WaveState {
    let path = root.join(".superwork").join(WAVE_FILENAME);
    if !path.exists() {
        return WaveState::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| toml::from_str(&c).ok())
        .unwrap_or_default()
}

fn save_wave(root: &Path, wave: &WaveState) -> Result<(), String> {
    let dir = root.join(".superwork");
    std::fs::create_dir_all(&dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let content = toml::to_string_pretty(wave).map_err(|e| format!("serializing wave: {e}"))?;
    let path = dir.join(WAVE_FILENAME);
    std::fs::write(&path, content).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(())
}

fn tier_status(wave: &WaveState, tier: usize) -> TierStatus {
    wave.tiers
        .get(&tier.to_string())
        .and_then(|t| match t.status.as_str() {
            "pending" => Some(TierStatus::Pending),
            "bumped" => Some(TierStatus::Bumped),
            "pushed" => Some(TierStatus::Pushed),
            "tagged" => Some(TierStatus::Tagged),
            "ci_watching" => Some(TierStatus::CiWatching),
            "ci_passed" => Some(TierStatus::CiPassed),
            "publishing" => Some(TierStatus::Publishing),
            "published" => Some(TierStatus::Published),
            _ => None,
        })
        .unwrap_or(TierStatus::Pending)
}

fn set_tier_status(wave: &mut WaveState, tier: usize, status: TierStatus, crates: &[String]) {
    let ts = wave.tiers.entry(tier.to_string()).or_default();
    ts.status = status.to_string();
    if ts.crates.is_empty() && !crates.is_empty() {
        ts.crates = crates.to_vec();
    }
}

// ── Wave orchestrator ──

fn run_wave(
    root: &Path,
    config: &SuperworkConfig,
    advance: bool,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;
    let mut wave = load_wave(root);

    if wave.started_at.is_empty() {
        wave.started_at = format_now();
        wave.workspace = config
            .meta()
            .name
            .as_deref()
            .unwrap_or("default")
            .to_string();
    }

    // Initialize tier crate lists
    for (idx, level) in levels.iter().enumerate() {
        let active: Vec<String> = level
            .iter()
            .filter(|name| {
                analyses
                    .get(name.as_str())
                    .is_some_and(|a| a.bump != "skip" && a.category != "skip" && !a.bump.is_empty())
            })
            .cloned()
            .collect();
        if !active.is_empty() && !wave.tiers.contains_key(&idx.to_string()) {
            set_tier_status(&mut wave, idx, TierStatus::Pending, &active);
        }
    }

    // Show current state
    println!("=== Release Wave ===");
    println!();
    for (idx, _level) in levels.iter().enumerate() {
        let status = tier_status(&wave, idx);
        let ts = wave.tiers.get(&idx.to_string());
        let count = ts.map(|t| t.crates.len()).unwrap_or(0);
        if count == 0 {
            continue;
        }
        let ci_info = ts
            .filter(|t| !t.ci_results.is_empty())
            .map(|t| {
                let passed = t.ci_results.values().filter(|v| *v == "success").count();
                format!(" (CI: {passed}/{} passed)", t.ci_results.len())
            })
            .unwrap_or_default();
        println!("  Tier {idx}: {status} — {count} crates{ci_info}");
    }
    println!();

    if !advance {
        println!("Run with --advance to execute the next step.");
        save_wave(root, &wave)?;
        return Ok(());
    }

    // Find the first non-published tier and advance it
    for (idx, _level) in levels.iter().enumerate() {
        let status = tier_status(&wave, idx);
        let ts = wave.tiers.get(&idx.to_string());
        let crates = ts.map(|t| t.crates.clone()).unwrap_or_default();
        if crates.is_empty() {
            continue;
        }

        match status {
            TierStatus::Published => continue,
            TierStatus::Pending => {
                println!("Tier {idx}: bumping versions...");
                if !dry_run {
                    run_bump(root, config, Some(idx), false)?;
                    // Commit + push in each repo
                    let tier_repos = tier_repo_paths(root, &eco, &crates);
                    for (repo_dir, repo_path) in &tier_repos {
                        let crate_list: Vec<&str> = crates
                            .iter()
                            .filter(|c| {
                                eco.crates
                                    .get(c.as_str())
                                    .is_some_and(|i| i.repo_dir == *repo_dir)
                            })
                            .map(|s| s.as_str())
                            .collect();
                        let msg = format!("release: bump {}", crate_list.join(", "));
                        let _ = Command::new("git")
                            .args(["add", "-A"])
                            .current_dir(repo_path)
                            .status();
                        let _ = Command::new("git")
                            .args(["commit", "-m", &msg])
                            .current_dir(repo_path)
                            .status();
                    }
                }
                set_tier_status(&mut wave, idx, TierStatus::Bumped, &crates);
                save_wave(root, &wave)?;
                println!("Tier {idx}: bumped. Run --advance again to push.");
                return Ok(());
            }
            TierStatus::Bumped => {
                println!("Tier {idx}: pushing to remotes...");
                if !dry_run {
                    let tier_repos = tier_repo_paths(root, &eco, &crates);
                    for (repo_dir, repo_path) in &tier_repos {
                        print!("  push {repo_dir}... ");
                        let ok = Command::new("git")
                            .args(["push"])
                            .current_dir(repo_path)
                            .status()
                            .map(|s| s.success())
                            .unwrap_or(false);
                        println!("{}", if ok { "ok" } else { "FAIL" });
                    }
                }
                set_tier_status(&mut wave, idx, TierStatus::Pushed, &crates);
                save_wave(root, &wave)?;
                println!("Tier {idx}: pushed. Run --advance again to tag.");
                return Ok(());
            }
            TierStatus::Pushed => {
                println!("Tier {idx}: creating tags + GH releases...");
                if !dry_run {
                    run_tag(root, config, idx, false)?;
                    // Capture CI run IDs
                    let tier_repos = tier_repo_paths(root, &eco, &crates);
                    let ts = wave.tiers.entry(idx.to_string()).or_default();
                    for (repo_dir, _) in &tier_repos {
                        let slug = config
                            .github_url_for(repo_dir)
                            .and_then(|u| u.strip_prefix("https://github.com/").map(String::from));
                        if let Some(slug) = slug {
                            if let Some(run_id) = get_latest_run_id(&slug) {
                                ts.ci_runs.insert(repo_dir.clone(), run_id);
                            }
                        }
                    }
                }
                set_tier_status(&mut wave, idx, TierStatus::Tagged, &crates);
                save_wave(root, &wave)?;
                println!("Tier {idx}: tagged. Run --advance to check CI.");
                return Ok(());
            }
            TierStatus::Tagged | TierStatus::CiWatching => {
                println!("Tier {idx}: checking CI status...");
                let tier_repos = tier_repo_paths(root, &eco, &crates);
                let ts = wave.tiers.entry(idx.to_string()).or_default();
                let global_allowlist = &config.release.ci_allow_failures_global;

                let mut all_done = true;
                let mut all_passed = true;
                for (repo_dir, _) in &tier_repos {
                    let slug = config
                        .github_url_for(repo_dir)
                        .and_then(|u| u.strip_prefix("https://github.com/").map(String::from));
                    let conclusion = slug
                        .as_ref()
                        .and_then(|s| get_run_conclusion(s))
                        .unwrap_or_default();

                    ts.ci_results.insert(repo_dir.clone(), conclusion.clone());

                    if conclusion.is_empty()
                        || conclusion == "null"
                        || conclusion == "in_progress"
                        || conclusion == "queued"
                    {
                        all_done = false;
                        print!("  {repo_dir}: pending");
                    } else if conclusion == "success" {
                        print!("  {repo_dir}: pass");
                    } else if glob_list_match(global_allowlist, &conclusion) {
                        print!("  {repo_dir}: {conclusion} (allowed)");
                    } else {
                        print!("  {repo_dir}: {conclusion}");
                        all_passed = false;
                    }
                    println!();
                }

                if all_done && all_passed {
                    set_tier_status(&mut wave, idx, TierStatus::CiPassed, &crates);
                    save_wave(root, &wave)?;
                    println!("Tier {idx}: CI passed! Run --advance to publish.");
                } else if all_done {
                    save_wave(root, &wave)?;
                    println!("Tier {idx}: CI has failures. Fix or add to ci_allow_failures.");
                } else {
                    set_tier_status(&mut wave, idx, TierStatus::CiWatching, &crates);
                    save_wave(root, &wave)?;
                    println!("Tier {idx}: CI still running. Run --advance again later.");
                }
                return Ok(());
            }
            TierStatus::CiPassed => {
                println!("Tier {idx}: publishing...");
                set_tier_status(&mut wave, idx, TierStatus::Publishing, &crates);
                save_wave(root, &wave)?;
                if !dry_run {
                    run_publish(root, config, idx, false)?;
                }
                set_tier_status(&mut wave, idx, TierStatus::Published, &crates);
                save_wave(root, &wave)?;
                println!("Tier {idx}: published!");
                return Ok(());
            }
            TierStatus::Publishing => {
                // Retry publish
                println!("Tier {idx}: retrying publish...");
                if !dry_run {
                    run_publish(root, config, idx, false)?;
                }
                set_tier_status(&mut wave, idx, TierStatus::Published, &crates);
                save_wave(root, &wave)?;
                println!("Tier {idx}: published!");
                return Ok(());
            }
        }
    }

    save_wave(root, &wave)?;
    println!("All tiers published! Wave complete.");
    Ok(())
}

/// Get unique repo paths for crates in a tier
fn tier_repo_paths(root: &Path, eco: &Ecosystem, crates: &[String]) -> Vec<(String, PathBuf)> {
    let mut seen = std::collections::BTreeSet::new();
    let mut result = Vec::new();
    for name in crates {
        if let Some(info) = eco.crates.get(name.as_str()) {
            if seen.insert(info.repo_dir.clone()) {
                result.push((
                    info.repo_dir.clone(),
                    resolve_repo_path(root, &info.repo_dir),
                ));
            }
        }
    }
    result
}

/// Get the latest CI run ID for a repo
fn get_latest_run_id(repo_slug: &str) -> Option<String> {
    let output = Command::new("gh")
        .args([
            "run",
            "list",
            "--repo",
            repo_slug,
            "--limit",
            "1",
            "--json",
            "databaseId",
            "--jq",
            ".[0].databaseId",
        ])
        .output()
        .ok()?;
    if output.status.success() {
        let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !id.is_empty() && id != "null" {
            return Some(id);
        }
    }
    None
}

/// Get CI conclusion for a repo's latest run
fn get_run_conclusion(repo_slug: &str) -> Option<String> {
    let output = Command::new("gh")
        .args([
            "run",
            "list",
            "--repo",
            repo_slug,
            "--limit",
            "1",
            "--json",
            "conclusion,status",
            "--jq",
            ".[0] | if .status == \"completed\" then .conclusion else .status end",
        ])
        .output()
        .ok()?;
    if output.status.success() {
        let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !s.is_empty() {
            return Some(s);
        }
    }
    None
}

// ── Commands ──

fn run_init(
    root: &Path,
    config: &SuperworkConfig,
    force: bool,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let ws_name = config
        .meta()
        .name
        .as_deref()
        .unwrap_or("default")
        .to_string();

    // Build tier map: crate_name → tier index
    let mut tier_map: BTreeMap<&str, usize> = BTreeMap::new();
    for (level_idx, level) in levels.iter().enumerate() {
        for name in level {
            tier_map.insert(name, level_idx);
        }
    }

    // Build dependent count map
    let mut dep_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for dep in &eco.deps {
        *dep_counts.entry(&dep.to_crate).or_default() += 1;
    }

    // Group publishable crates by repo
    let mut by_repo: BTreeMap<&str, Vec<&CrateInfo>> = BTreeMap::new();
    for info in eco.crates.values() {
        if info.publishable {
            by_repo.entry(&info.repo_dir).or_default().push(info);
        }
    }

    let now = format_now();
    let mut repos_written = 0;
    let mut crates_analyzed = 0;

    for (repo_dir, repo_crates) in &by_repo {
        let repo_path = resolve_repo_path(root, repo_dir);
        let analysis_dir = repo_path.join(".superwork");
        let analysis_path = analysis_dir.join(ANALYSIS_FILENAME);

        // Skip if exists and not forced
        if analysis_path.exists() && !force {
            continue;
        }

        let mut analysis = RepoAnalysis {
            schema_version: SCHEMA_VERSION,
            superworkspace: ws_name.clone(),
            generated_at: now.clone(),
            crates: BTreeMap::new(),
        };

        for info in repo_crates {
            let crate_analysis = build_crate_analysis(info, &repo_path, &tier_map, &dep_counts)?;
            analysis.crates.insert(info.name.clone(), crate_analysis);
            crates_analyzed += 1;
        }

        if !dry_run {
            std::fs::create_dir_all(&analysis_dir)
                .map_err(|e| format!("creating {}: {e}", analysis_dir.display()))?;
            let content = toml::to_string_pretty(&analysis)
                .map_err(|e| format!("serializing analysis: {e}"))?;
            std::fs::write(&analysis_path, content)
                .map_err(|e| format!("writing {}: {e}", analysis_path.display()))?;
        }

        let label = if dry_run { "[dry-run] " } else { "" };
        println!("{label}{repo_dir}: {} crates analyzed", repo_crates.len());
        repos_written += 1;
    }

    let label = if dry_run { "[dry-run] " } else { "" };
    println!("{label}Generated analysis for {crates_analyzed} crates across {repos_written} repos");
    Ok(())
}

fn run_analyze(
    root: &Path,
    config: &SuperworkConfig,
    tier_filter: Option<usize>,
    uncategorized_only: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;

    // Build dependent count map
    let mut dep_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for dep in &eco.deps {
        *dep_counts.entry(&dep.to_crate).or_default() += 1;
    }

    for (level_idx, level) in levels.iter().enumerate() {
        if let Some(t) = tier_filter {
            if level_idx != t {
                continue;
            }
        }

        let mut tier_crates: Vec<(&str, Option<&CrateAnalysis>)> = Vec::new();
        for name in level {
            let ca = analyses.get(name.as_str());
            if uncategorized_only && ca.is_some_and(|a| !a.category.is_empty()) {
                continue;
            }
            tier_crates.push((name, ca));
        }

        if tier_crates.is_empty() {
            continue;
        }

        println!("=== Tier {level_idx} ({} crates) ===", tier_crates.len());
        println!();

        for (name, ca) in &tier_crates {
            let info = &eco.crates[*name];
            let deps = dep_counts.get(*name).copied().unwrap_or(0);

            println!(
                "{name} ({}, {}, {deps} dependents)",
                info.version, info.class
            );

            if let Some(a) = ca {
                if a.tag_found {
                    println!("  tag: {} → HEAD ({} commits)", a.tag, a.commit_count);
                } else {
                    println!("  tag: none (never tagged)");
                }
                if a.files_changed > 0 {
                    println!(
                        "  diff: +{} -{} across {} files ({} src)",
                        a.lines_added, a.lines_removed, a.files_changed, a.src_files_changed
                    );
                }
                if !a.commit_subjects.is_empty() {
                    println!("  commits:");
                    for s in a.commit_subjects.iter().take(10) {
                        println!("    - {s}");
                    }
                    if a.commit_subjects.len() > 10 {
                        println!("    ... and {} more", a.commit_subjects.len() - 10);
                    }
                }

                if a.category.is_empty() {
                    println!("  [awaiting: category, bump]");
                } else {
                    println!(
                        "  categorized: {} / {} — {}",
                        a.category,
                        a.bump,
                        if a.reason.is_empty() {
                            "(no reason)"
                        } else {
                            &a.reason
                        }
                    );
                }
            } else {
                println!("  [no analysis file — run `release init`]");
            }
            println!();
        }
    }

    Ok(())
}

fn run_categorize(
    root: &Path,
    config: &SuperworkConfig,
    crate_name: &str,
    category: &str,
    bump: &str,
    reason: Option<&str>,
) -> Result<(), String> {
    // Validate inputs
    let valid_categories = [
        "breaking", "feature", "fix", "perf", "docs", "deps", "internal", "skip",
    ];
    if !valid_categories.contains(&category) {
        return Err(format!(
            "invalid category '{category}'. Valid: {}",
            valid_categories.join(", ")
        ));
    }
    let valid_bumps = ["major", "minor", "patch", "skip"];
    if !valid_bumps.contains(&bump) {
        return Err(format!(
            "invalid bump '{bump}'. Valid: {}",
            valid_bumps.join(", ")
        ));
    }

    let eco = discover::scan_ecosystem(root, config)?;
    let info = eco
        .crates
        .get(crate_name)
        .ok_or_else(|| format!("crate '{crate_name}' not found"))?;

    let repo_path = resolve_repo_path(root, &info.repo_dir);
    let analysis_path = repo_path.join(".superwork").join(ANALYSIS_FILENAME);

    if !analysis_path.exists() {
        return Err(format!(
            "no analysis file at {}. Run `release init` first.",
            analysis_path.display()
        ));
    }

    let content = std::fs::read_to_string(&analysis_path)
        .map_err(|e| format!("reading {}: {e}", analysis_path.display()))?;
    let mut analysis: RepoAnalysis = toml::from_str(&content)
        .map_err(|e| format!("parsing {}: {e}", analysis_path.display()))?;

    let ca = analysis
        .crates
        .get_mut(crate_name)
        .ok_or_else(|| format!("crate '{crate_name}' not in analysis file"))?;

    ca.category = category.to_string();
    ca.bump = bump.to_string();
    ca.reason = reason.unwrap_or("").to_string();

    let new_content = toml::to_string_pretty(&analysis).map_err(|e| format!("serializing: {e}"))?;
    std::fs::write(&analysis_path, new_content)
        .map_err(|e| format!("writing {}: {e}", analysis_path.display()))?;

    println!("Categorized {crate_name}: {category} / {bump}");
    Ok(())
}

fn run_status(root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;

    println!(
        "{:<30} {:>4} {:>8} {:>10} {:>8} {:>7} {:>7}",
        "Crate", "Tier", "Class", "Category", "Bump", "Semver", "Copter"
    );
    println!("{}", "-".repeat(86));

    for (level_idx, level) in levels.iter().enumerate() {
        for name in level {
            let info = &eco.crates[name];
            let ca = analyses.get(name.as_str());

            let category = ca.map(|a| a.category.as_str()).unwrap_or("");
            let bump = ca.map(|a| a.bump.as_str()).unwrap_or("");
            let semver = ca.map(|a| a.semver_check.as_str()).unwrap_or("");
            let copter = ca.map(|a| a.copter.as_str()).unwrap_or("");

            println!(
                "{:<30} {:>4} {:>8} {:>10} {:>8} {:>7} {:>7}",
                name, level_idx, info.class, category, bump, semver, copter
            );
        }
    }

    // Summary
    let total = analyses.len();
    let categorized = analyses.values().filter(|a| !a.category.is_empty()).count();
    let skipped = analyses
        .values()
        .filter(|a| a.bump == "skip" || a.category == "skip")
        .count();

    println!();
    println!(
        "{total} crates | {categorized} categorized | {skipped} skipped | {} pending",
        total - categorized
    );

    Ok(())
}

fn run_next(root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;

    // Check if init has been run
    if analyses.is_empty() {
        println!("No analysis files found. Run: cargo superwork release init");
        return Ok(());
    }

    // Find uncategorized crates
    let uncategorized: Vec<&str> = analyses
        .iter()
        .filter(|(_, a)| a.category.is_empty())
        .map(|(name, _)| *name)
        .collect();

    if !uncategorized.is_empty() {
        println!(
            "Categorize {} crates. Run: cargo superwork release analyze --uncategorized",
            uncategorized.len()
        );
        println!();
        println!("For each, run:");
        println!(
            "  cargo superwork release categorize <CRATE> --category <CAT> --bump <LEVEL> --reason \"...\""
        );
        return Ok(());
    }

    // Find tiers that need semver checks
    for (level_idx, level) in levels.iter().enumerate() {
        let needs_semver: Vec<&str> = level
            .iter()
            .filter(|name| {
                analyses.get(name.as_str()).is_some_and(|a| {
                    a.bump != "skip"
                        && a.category != "skip"
                        && a.semver_check.is_empty()
                        && eco
                            .crates
                            .get(name.as_str())
                            .is_some_and(|c| c.class == CrateClass::Library)
                })
            })
            .map(|s| s.as_str())
            .collect();

        if !needs_semver.is_empty() {
            println!(
                "Tier {level_idx}: {} crates need semver-checks. Run:",
                needs_semver.len()
            );
            println!("  cargo superwork release check --tier {level_idx}");
            return Ok(());
        }
    }

    // If everything is categorized and checked, suggest publish
    for (level_idx, level) in levels.iter().enumerate() {
        let ready: Vec<&str> = level
            .iter()
            .filter(|name| {
                analyses
                    .get(name.as_str())
                    .is_some_and(|a| a.bump != "skip" && a.category != "skip")
            })
            .map(|s| s.as_str())
            .collect();

        if !ready.is_empty() {
            println!(
                "Tier {level_idx}: {} crates ready. Next steps:",
                ready.len()
            );
            println!("  1. cargo superwork release local-test --tier {level_idx}");
            println!("  2. Create tags + GH releases for tier {level_idx}");
            println!("  3. Wait for CI to pass");
            println!("  4. cargo superwork release publish {level_idx}");
            return Ok(());
        }
    }

    println!("All tiers processed. Release complete or all crates skipped.");
    Ok(())
}

// ── Phase 3: Check + Local Test ──

fn run_check(
    root: &Path,
    config: &SuperworkConfig,
    tier_filter: Option<usize>,
    run_copter: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;

    let mut checked = 0;
    let mut breaking = 0;
    let mut copter_pass = 0;
    let mut copter_fail = 0;

    for (level_idx, level) in levels.iter().enumerate() {
        if let Some(t) = tier_filter {
            if level_idx != t {
                continue;
            }
        }

        for name in level {
            let Some(ca) = analyses.get(name.as_str()) else {
                continue;
            };
            if ca.bump == "skip" || ca.category == "skip" {
                continue;
            }

            let info = &eco.crates[name];

            // Semver-checks for Library crates only
            if info.class == CrateClass::Library {
                let crate_dir = info.manifest_path.parent().unwrap();
                let repo_dir = if let Some(ws) = &info.workspace_root {
                    ws.parent().unwrap()
                } else {
                    crate_dir
                };

                print!("  semver-check {name}... ");
                let output = Command::new("cargo")
                    .args(["semver-checks"])
                    .current_dir(repo_dir)
                    .output();

                let result = match &output {
                    Ok(o) if o.status.success() => {
                        println!("pass");
                        "pass".to_string()
                    }
                    Ok(o) if o.status.code() == Some(1) => {
                        println!("BREAKING");
                        breaking += 1;
                        "breaking".to_string()
                    }
                    Ok(o) => {
                        let stderr = String::from_utf8_lossy(&o.stderr);
                        println!("error: {}", stderr.lines().next().unwrap_or("unknown"));
                        format!("error: {}", stderr.lines().next().unwrap_or(""))
                    }
                    Err(e) => {
                        println!("error: {e}");
                        format!("error: {e}")
                    }
                };

                update_analysis_field(root, info, |a| a.semver_check = result)?;
                checked += 1;
            }

            // Copter for crates with dependents
            if run_copter && ca.dependents > 0 {
                print!("  copter {name} ({} dependents)... ", ca.dependents);

                // Find reverse dep directories
                let rev_deps: Vec<PathBuf> = eco
                    .deps
                    .iter()
                    .filter(|d| d.to_crate == *name)
                    .filter_map(|d| {
                        eco.crates
                            .get(&d.from_crate)
                            .map(|c| c.manifest_path.parent().unwrap().to_path_buf())
                    })
                    .collect();

                if rev_deps.is_empty() {
                    println!("skipped (no local reverse deps)");
                    update_analysis_field(root, info, |a| {
                        a.copter = "skipped".to_string();
                    })?;
                } else {
                    let crate_dir = info.manifest_path.parent().unwrap();
                    let mut args = vec![
                        "copter".to_string(),
                        "-p".to_string(),
                        crate_dir.to_string_lossy().to_string(),
                        "--only-check".to_string(),
                        "--simple".to_string(),
                    ];
                    for rd in &rev_deps {
                        args.push("--dependent-paths".to_string());
                        args.push(rd.to_string_lossy().to_string());
                    }

                    let output = Command::new("cargo").args(&args).current_dir(root).output();

                    let result = match &output {
                        Ok(o) if o.status.success() => {
                            println!("pass");
                            copter_pass += 1;
                            "pass".to_string()
                        }
                        Ok(_) => {
                            println!("FAIL");
                            copter_fail += 1;
                            "fail".to_string()
                        }
                        Err(e) => {
                            println!("error: {e}");
                            format!("error: {e}")
                        }
                    };

                    update_analysis_field(root, info, |a| a.copter = result)?;
                }
            }
        }
    }

    println!();
    println!("Semver: {checked} checked, {breaking} breaking");
    if run_copter {
        println!("Copter: {copter_pass} pass, {copter_fail} fail");
    }
    Ok(())
}

fn run_local_test(
    root: &Path,
    config: &SuperworkConfig,
    tier_filter: Option<usize>,
    target: Option<&str>,
    lint_only: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;

    // Determine targets: explicit, from config, or just native
    let cross_targets: Vec<&str> = if let Some(t) = target {
        vec![t]
    } else {
        config
            .release
            .local_targets
            .iter()
            .map(|s| s.as_str())
            .collect()
    };

    let mut pass = 0;
    let mut fail = 0;

    for (level_idx, level) in levels.iter().enumerate() {
        if let Some(t) = tier_filter {
            if level_idx != t {
                continue;
            }
        }

        // Group by repo to avoid running tests multiple times per repo
        let mut tier_repos: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for name in level {
            let Some(ca) = analyses.get(name.as_str()) else {
                continue;
            };
            if ca.bump == "skip" || ca.category == "skip" {
                continue;
            }
            let info = &eco.crates[name];
            tier_repos.entry(&info.repo_dir).or_default().push(name);
        }

        for (repo_dir, crate_names) in &tier_repos {
            let repo_path = resolve_repo_path(root, repo_dir);
            println!("=== {repo_dir} ({}) ===", crate_names.join(", "));

            // Clippy
            print!("  clippy... ");
            if run_cmd_ok(
                &repo_path,
                "cargo",
                &["clippy", "--all-targets", "--", "-D", "warnings"],
            ) {
                println!("pass");
            } else {
                println!("FAIL");
                fail += 1;
            }

            // Fmt
            print!("  fmt... ");
            if run_cmd_ok(&repo_path, "cargo", &["fmt", "--", "--check"]) {
                println!("pass");
            } else {
                println!("FAIL");
                fail += 1;
            }

            if lint_only {
                pass += 1;
                continue;
            }

            // Native test
            print!("  test (native)... ");
            if run_cmd_ok(&repo_path, "cargo", &["test"]) {
                println!("pass");
                pass += 1;
            } else {
                println!("FAIL");
                fail += 1;
            }

            // Cross targets
            for target in &cross_targets {
                print!("  test ({target})... ");
                if run_cmd_ok(&repo_path, "cross", &["test", "--target", target]) {
                    println!("pass");
                } else {
                    println!("FAIL");
                    fail += 1;
                }
            }
        }
    }

    println!();
    println!("{pass} repos passed, {fail} failures");
    Ok(())
}

// ── Phase 4: Bump, Tag, CI Status, Publish ──

fn run_bump(
    root: &Path,
    config: &SuperworkConfig,
    tier_filter: Option<usize>,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;

    let mut bumped = 0;

    for (level_idx, level) in levels.iter().enumerate() {
        if let Some(t) = tier_filter {
            if level_idx != t {
                continue;
            }
        }

        for name in level {
            let Some(ca) = analyses.get(name.as_str()) else {
                continue;
            };
            if ca.bump == "skip" || ca.category == "skip" || ca.bump.is_empty() {
                continue;
            }

            let info = &eco.crates[name];
            let current = semver::Version::parse(&info.version)
                .map_err(|e| format!("{name}: bad version '{}': {e}", info.version))?;

            let new_version = match ca.bump.as_str() {
                "major" => format!("{}.0.0", current.major + 1),
                "minor" => format!("{}.{}.0", current.major, current.minor + 1),
                "patch" => format!("{}.{}.{}", current.major, current.minor, current.patch + 1),
                other => return Err(format!("{name}: invalid bump level '{other}'")),
            };

            let label = if dry_run { "[dry-run] " } else { "" };
            println!(
                "{label}{name}: {} → {new_version} ({} {})",
                info.version, ca.category, ca.bump
            );

            if !dry_run {
                crate::bump::run(root, config, name, &new_version, false)?;
            }
            bumped += 1;
        }
    }

    let label = if dry_run { "[dry-run] " } else { "" };
    println!("{label}Bumped {bumped} crates");
    Ok(())
}

fn run_tag(
    root: &Path,
    config: &SuperworkConfig,
    tier: usize,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;

    let level = levels
        .get(tier)
        .ok_or_else(|| format!("tier {tier} does not exist (max: {})", levels.len() - 1))?;

    let mut tagged = 0;

    for name in level {
        let Some(ca) = analyses.get(name.as_str()) else {
            continue;
        };
        if ca.bump == "skip" || ca.category == "skip" {
            continue;
        }

        let info = &eco.crates[name];
        let repo_path = resolve_repo_path(root, &info.repo_dir);
        let tag = format!("{}-v{}", info.name, info.version);

        let label = if dry_run { "[dry-run] " } else { "" };

        // Check if tag already exists
        if git_output(
            &repo_path,
            &["rev-parse", "--verify", &format!("refs/tags/{tag}")],
        )
        .is_some()
        {
            println!("{label}{name}: tag {tag} already exists, skipping");
            continue;
        }

        println!("{label}{name}: creating tag {tag}");

        if !dry_run {
            // Create tag
            let status = Command::new("git")
                .args(["tag", &tag])
                .current_dir(&repo_path)
                .status()
                .map_err(|e| format!("git tag {tag}: {e}"))?;
            if !status.success() {
                return Err(format!("git tag {tag} failed"));
            }

            // Push tag
            let status = Command::new("git")
                .args(["push", "origin", &tag])
                .current_dir(&repo_path)
                .status()
                .map_err(|e| format!("git push origin {tag}: {e}"))?;
            if !status.success() {
                return Err(format!("git push origin {tag} failed"));
            }

            // Create GitHub release
            let gh_repo = config
                .github_url_for(&info.repo_dir)
                .and_then(|url| url.strip_prefix("https://github.com/").map(String::from));

            if let Some(repo_slug) = gh_repo {
                let status = Command::new("gh")
                    .args([
                        "release",
                        "create",
                        &tag,
                        "--repo",
                        &repo_slug,
                        "--title",
                        &tag,
                        "--generate-notes",
                    ])
                    .current_dir(&repo_path)
                    .status()
                    .map_err(|e| format!("gh release create {tag}: {e}"))?;
                if !status.success() {
                    eprintln!("  warning: gh release create {tag} failed (tag was pushed)");
                }
            }
        }

        tagged += 1;
    }

    let label = if dry_run { "[dry-run] " } else { "" };
    println!("{label}Tagged {tagged} crates in tier {tier}");
    Ok(())
}

fn run_ci_status(
    root: &Path,
    config: &SuperworkConfig,
    tier_filter: Option<usize>,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;

    let global_allowlist = &config.release.ci_allow_failures_global;

    for (level_idx, level) in levels.iter().enumerate() {
        if let Some(t) = tier_filter {
            if level_idx != t {
                continue;
            }
        }

        // Group by repo
        let mut tier_repos: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for name in level {
            let Some(ca) = analyses.get(name.as_str()) else {
                continue;
            };
            if ca.bump == "skip" || ca.category == "skip" {
                continue;
            }
            let info = &eco.crates[name];
            tier_repos.entry(&info.repo_dir).or_default().push(name);
        }

        if tier_repos.is_empty() {
            continue;
        }

        println!("=== Tier {level_idx} CI Status ===");

        for (repo_dir, crate_names) in &tier_repos {
            let gh_url = config.github_url_for(repo_dir);
            let repo_slug = gh_url
                .as_ref()
                .and_then(|url| url.strip_prefix("https://github.com/"));

            let Some(slug) = repo_slug else {
                println!("  {repo_dir}: no GitHub URL configured");
                continue;
            };

            // Get latest run status
            let output = Command::new("gh")
                .args([
                    "run",
                    "list",
                    "--repo",
                    slug,
                    "--limit",
                    "1",
                    "--json",
                    "conclusion,name",
                    "--jq",
                    ".[0].conclusion",
                ])
                .output();

            let conclusion = output
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_else(|| "unknown".to_string());

            let is_allowed_failure = conclusion != "success"
                && (glob_list_match(global_allowlist, &conclusion)
                    || crate_names.iter().any(|name| {
                        config
                            .release
                            .ci_allow_failures
                            .get(*name)
                            .is_some_and(|list| glob_list_match(list, &conclusion))
                    }));

            let status_str = if conclusion == "success" {
                "pass"
            } else if is_allowed_failure {
                "allowed-failure"
            } else if conclusion.is_empty() || conclusion == "null" {
                "pending"
            } else {
                "BLOCKING"
            };

            println!(
                "  {repo_dir} ({}): {conclusion} → {status_str}",
                crate_names.join(", ")
            );
        }
    }

    Ok(())
}

fn run_publish(
    root: &Path,
    config: &SuperworkConfig,
    tier: usize,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let levels = graph::publish_order(&eco, true)?;
    let analyses = load_all_analyses(root, &eco)?;

    let level = levels
        .get(tier)
        .ok_or_else(|| format!("tier {tier} does not exist (max: {})", levels.len() - 1))?;

    let wait_secs = config.release.index_wait_secs;
    let label = if dry_run { "[dry-run] " } else { "" };

    let mut published = 0;
    let mut failed = 0;

    for name in level {
        let Some(ca) = analyses.get(name.as_str()) else {
            continue;
        };
        if ca.bump == "skip" || ca.category == "skip" {
            continue;
        }

        let info = &eco.crates[name];
        let crate_dir = info.manifest_path.parent().unwrap();

        println!("{label}Publishing {name} {}...", info.version);

        if dry_run {
            published += 1;
            continue;
        }

        let status = Command::new("cargo")
            .args(["publish"])
            .current_dir(crate_dir)
            .status()
            .map_err(|e| format!("cargo publish {name}: {e}"))?;

        if !status.success() {
            eprintln!("  ERROR: cargo publish {name} failed");
            failed += 1;
            continue;
        }

        published += 1;

        // Wait for index propagation
        if wait_secs > 0 {
            println!("  waiting {wait_secs}s for index propagation...");
            std::thread::sleep(std::time::Duration::from_secs(wait_secs));
        }

        // Verify on crates.io (3 attempts)
        let mut verified = false;
        for attempt in 1..=3 {
            let output = Command::new("cargo").args(["info", name]).output();

            if let Ok(o) = output {
                let stdout = String::from_utf8_lossy(&o.stdout);
                if stdout.contains(&info.version) {
                    println!("  verified on crates.io (attempt {attempt})");
                    verified = true;
                    break;
                }
            }

            if attempt < 3 {
                println!("  not yet visible, retrying in 10s...");
                std::thread::sleep(std::time::Duration::from_secs(10));
            }
        }

        if !verified {
            eprintln!(
                "  WARNING: could not verify {name} {} on crates.io",
                info.version
            );
        }
    }

    println!("{label}Tier {tier}: {published} published, {failed} failed");
    Ok(())
}

// ── Helpers ──

fn build_crate_analysis(
    info: &CrateInfo,
    repo_path: &Path,
    tier_map: &BTreeMap<&str, usize>,
    dep_counts: &BTreeMap<&str, usize>,
) -> Result<CrateAnalysis, String> {
    let mut ca = CrateAnalysis {
        version: info.version.clone(),
        class: info.class.to_string(),
        tier: tier_map.get(info.name.as_str()).copied(),
        dependents: dep_counts.get(info.name.as_str()).copied().unwrap_or(0),
        ..Default::default()
    };

    // Get HEAD commit
    ca.head = git_output(repo_path, &["rev-parse", "--short", "HEAD"])
        .unwrap_or_default()
        .trim()
        .to_string();

    // Find version tag
    if let Some(tag) = find_tag(info, repo_path) {
        ca.tag = tag.clone();
        ca.tag_found = true;

        // Diff stats
        let range = format!("{tag}..HEAD");
        let crate_dir = info.manifest_path.parent().unwrap();
        let rel_prefix = crate_dir.strip_prefix(repo_path).ok().and_then(|p| {
            let s = p.to_string_lossy();
            if s.is_empty() {
                None
            } else {
                Some(format!("{s}/"))
            }
        });

        // File count and line stats
        let diff_args = if let Some(ref prefix) = rel_prefix {
            vec!["diff", "--numstat", &range, "--", prefix.as_str()]
        } else {
            vec!["diff", "--numstat", &range, "--", "."]
        };

        if let Some(numstat) = git_output(repo_path, &diff_args) {
            for line in numstat.lines() {
                let parts: Vec<&str> = line.split('\t').collect();
                if parts.len() >= 3 {
                    ca.files_changed += 1;
                    ca.lines_added += parts[0].parse::<usize>().unwrap_or(0);
                    ca.lines_removed += parts[1].parse::<usize>().unwrap_or(0);
                    if parts[2].contains("src/") {
                        ca.src_files_changed += 1;
                    }
                }
            }
        }

        // Commit subjects
        let log_args = if let Some(ref prefix) = rel_prefix {
            vec![
                "log",
                "--oneline",
                "--format=%s",
                &range,
                "--",
                prefix.as_str(),
            ]
        } else {
            vec!["log", "--oneline", "--format=%s", &range, "--", "."]
        };

        if let Some(log) = git_output(repo_path, &log_args) {
            ca.commit_subjects = log.lines().map(|l| l.to_string()).collect();
            ca.commit_count = ca.commit_subjects.len();
        }
    }

    Ok(ca)
}

fn find_tag(info: &CrateInfo, repo_path: &Path) -> Option<String> {
    let candidates = [
        format!("{}-v{}", info.name, info.version),
        format!("v{}", info.version),
        format!("{}-{}", info.name, info.version),
    ];

    for tag in &candidates {
        if let Some(output) = git_output(
            repo_path,
            &["rev-parse", "--verify", &format!("refs/tags/{tag}")],
        ) {
            if !output.trim().is_empty() {
                return Some(tag.clone());
            }
        }
    }
    None
}

fn git_output(dir: &Path, args: &[&str]) -> Option<String> {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
}

fn resolve_repo_path(root: &Path, repo_dir: &str) -> PathBuf {
    let candidate = root.join(repo_dir);
    candidate.canonicalize().unwrap_or(candidate)
}

fn load_all_analyses<'a>(
    root: &Path,
    eco: &'a Ecosystem,
) -> Result<BTreeMap<&'a str, CrateAnalysis>, String> {
    let mut result: BTreeMap<&str, CrateAnalysis> = BTreeMap::new();

    // Group crates by repo to avoid re-reading the same file
    let mut by_repo: BTreeMap<&str, Vec<&CrateInfo>> = BTreeMap::new();
    for info in eco.crates.values() {
        if info.publishable {
            by_repo.entry(&info.repo_dir).or_default().push(info);
        }
    }

    for (repo_dir, repo_crates) in &by_repo {
        let repo_path = resolve_repo_path(root, repo_dir);
        let analysis_path = repo_path.join(".superwork").join(ANALYSIS_FILENAME);

        if !analysis_path.exists() {
            continue;
        }

        let content = std::fs::read_to_string(&analysis_path)
            .map_err(|e| format!("reading {}: {e}", analysis_path.display()))?;
        let analysis: RepoAnalysis = toml::from_str(&content)
            .map_err(|e| format!("parsing {}: {e}", analysis_path.display()))?;

        for info in repo_crates {
            if let Some(ca) = analysis.crates.get(&info.name) {
                result.insert(&info.name, ca.clone());
            }
        }
    }

    Ok(result)
}

fn format_now() -> String {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    format!("{secs}")
}

/// Update a single field in a crate's analysis file
fn update_analysis_field(
    root: &Path,
    info: &CrateInfo,
    update: impl FnOnce(&mut CrateAnalysis),
) -> Result<(), String> {
    let repo_path = resolve_repo_path(root, &info.repo_dir);
    let analysis_path = repo_path.join(".superwork").join(ANALYSIS_FILENAME);

    if !analysis_path.exists() {
        return Ok(()); // No analysis file to update
    }

    let content = std::fs::read_to_string(&analysis_path)
        .map_err(|e| format!("reading {}: {e}", analysis_path.display()))?;
    let mut analysis: RepoAnalysis = toml::from_str(&content)
        .map_err(|e| format!("parsing {}: {e}", analysis_path.display()))?;

    if let Some(ca) = analysis.crates.get_mut(&info.name) {
        update(ca);
    }

    let new_content = toml::to_string_pretty(&analysis).map_err(|e| format!("serializing: {e}"))?;
    std::fs::write(&analysis_path, new_content)
        .map_err(|e| format!("writing {}: {e}", analysis_path.display()))?;

    Ok(())
}

/// Run a command and return whether it succeeded
fn run_cmd_ok(dir: &Path, program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check if any pattern in the list matches the value (simple glob)
fn glob_list_match(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|p| {
        if p == "*" {
            true
        } else if let Some(suffix) = p.strip_prefix('*') {
            value.ends_with(suffix)
        } else if let Some(prefix) = p.strip_suffix('*') {
            value.starts_with(prefix)
        } else {
            p == value
        }
    })
}
