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
    // Simple RFC 3339 without chrono dependency
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    // Good enough for timestamps — not worth adding chrono
    format!("{secs}")
}
