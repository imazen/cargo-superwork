use crate::ci;
use crate::config::{CiStrategy, SuperworkConfig};
use crate::discover::{self, CrateInfo, DepSection, Ecosystem, InternalDep};
use std::collections::BTreeMap;
use std::path::Path;

// ── Types ──

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Severity {
    Pass,
    Info,
    Warn,
    Error,
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pass => write!(f, "PASS "),
            Self::Info => write!(f, "INFO "),
            Self::Warn => write!(f, "WARN "),
            Self::Error => write!(f, "ERROR"),
        }
    }
}

struct DepFinding {
    from_crate: String,
    to_crate: String,
    section: DepSection,
    strategy: CiStrategy,
    severity: Severity,
    message: String,
}

struct RepoReport {
    repo_dir: String,
    findings: Vec<DepFinding>,
    sed_hacks: Vec<String>,
    has_ci_config: bool,
    uses_ci_prep: bool,
}

struct LintReport {
    repos: Vec<RepoReport>,
    total_deps: usize,
}

// ── Entry point ──

pub fn run(
    ecosystem_root: &Path,
    config: &SuperworkConfig,
    filter: Option<&str>,
    online: bool,
    verbose: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;

    // Optionally query crates.io
    let published: BTreeMap<String, Option<String>> = if online {
        let names: Vec<String> = eco
            .crates
            .values()
            .filter(|c| c.publishable)
            .map(|c| c.name.clone())
            .collect();
        eprintln!("querying crates.io for {} crates...", names.len());
        query_published_versions(&names)
    } else {
        BTreeMap::new()
    };

    let report = build_report(&eco, config, ecosystem_root, &published, filter);
    let errors = print_report(&report, verbose, online);

    if errors > 0 {
        Err(format!("{errors} CI lint errors found"))
    } else {
        Ok(())
    }
}

// ── Report building ──

fn build_report(
    eco: &Ecosystem,
    config: &SuperworkConfig,
    ecosystem_root: &Path,
    published: &BTreeMap<String, Option<String>>,
    filter: Option<&str>,
) -> LintReport {
    // Group deps by from_crate's repo
    let mut by_repo: BTreeMap<String, Vec<&InternalDep>> = BTreeMap::new();
    for dep in &eco.deps {
        if !dep.has_path {
            continue; // Only path deps get transformed by ci-prep
        }
        if let Some(info) = eco.crates.get(&dep.from_crate) {
            by_repo
                .entry(info.repo_dir.clone())
                .or_default()
                .push(dep);
        }
    }

    // Apply filter
    if let Some(pattern) = filter {
        by_repo.retain(|repo_dir, _| {
            eco.crates
                .values()
                .any(|c| c.repo_dir == *repo_dir && glob_match(pattern, &c.name))
        });
    }

    let mut total_deps = 0;
    let mut repos = Vec::new();

    for (repo_dir, deps) in &by_repo {
        total_deps += deps.len();

        // Get inline CI config for this repo's crates
        let repo_crates: Vec<&CrateInfo> = eco
            .crates
            .values()
            .filter(|c| c.repo_dir == *repo_dir)
            .collect();

        let has_ci_config = repo_crates.iter().any(|c| c.inline_ci.is_some());

        // Check if CI workflow calls ci-prep
        let repo_path = resolve_repo_path(ecosystem_root, repo_dir, &repo_crates);
        let uses_ci_prep = check_uses_ci_prep(&repo_path);
        let sed_hacks = scan_for_sed_hacks(&repo_path);

        // Check for [patch.crates-io] sections
        let has_patch_section = repo_crates
            .iter()
            .any(|c| manifest_has_patch_section(&c.manifest_path));
        let patch_section_deleted = repo_crates.iter().any(|c| {
            c.inline_ci
                .as_ref()
                .is_some_and(|ci| ci.delete_sections.iter().any(|s| s.contains("patch")))
        });

        // Classify each dep
        let mut findings = Vec::new();
        for dep in deps {
            let inline_ci = eco
                .crates
                .get(&dep.from_crate)
                .and_then(|c| c.inline_ci.as_ref());

            let finding = classify_dep(dep, eco, config, published, inline_ci);
            findings.push(finding);
        }

        // Add patch section warning
        if has_patch_section && !patch_section_deleted {
            findings.push(DepFinding {
                from_crate: repo_dir.clone(),
                to_crate: String::new(),
                section: DepSection::Dependencies,
                strategy: CiStrategy::GitUrl,
                severity: Severity::Warn,
                message: "[patch.crates-io] exists but not in delete_sections — will break CI resolution".into(),
            });
        }

        // Sort: errors first, then warnings, then info, then pass
        findings.sort_by(|a, b| b.severity.cmp(&a.severity));

        repos.push(RepoReport {
            repo_dir: repo_dir.clone(),
            findings,
            sed_hacks,
            has_ci_config,
            uses_ci_prep,
        });
    }

    repos.sort_by(|a, b| {
        let a_max = a
            .findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or(Severity::Pass);
        let b_max = b
            .findings
            .iter()
            .map(|f| f.severity)
            .max()
            .unwrap_or(Severity::Pass);
        b_max.cmp(&a_max).then(a.repo_dir.cmp(&b.repo_dir))
    });

    LintReport { repos, total_deps }
}

// ── Core decision tree ──

fn classify_dep(
    dep: &InternalDep,
    eco: &Ecosystem,
    config: &SuperworkConfig,
    published: &BTreeMap<String, Option<String>>,
    inline_ci: Option<&crate::config::CiCrateOverride>,
) -> DepFinding {
    let strategy = config.ci_strategy_for(&dep.from_crate, &dep.to_crate, inline_ci);
    let to_info = eco.crates.get(&dep.to_crate);

    let (severity, message) = match strategy {
        CiStrategy::Delete => {
            let is_required = dep.section == DepSection::Dependencies && !dep.is_optional;
            if is_required {
                (
                    Severity::Warn,
                    "required dep deleted in CI — features using it won't compile".into(),
                )
            } else {
                (Severity::Pass, "deleted in CI".into())
            }
        }

        CiStrategy::StripPath => {
            if !dep.has_version {
                (
                    Severity::Error,
                    "strip_path but no version — CI resolution will fail".into(),
                )
            } else {
                let version_req_str = dep.version_value.as_deref().unwrap_or("*");
                classify_strip_path(version_req_str, &dep.to_crate, to_info, published)
            }
        }

        CiStrategy::GitUrl => {
            let git_url = ci::dep_git_url(&dep.to_crate, eco, config);
            if git_url.is_none() {
                (
                    Severity::Error,
                    format!("git_url strategy but no GitHub URL for '{}'", dep.to_crate),
                )
            } else if dep.has_version {
                // Version alongside git URL — git takes precedence but version mismatch is a signal
                if let (Some(req_str), Some(info)) = (&dep.version_value, to_info) {
                    match check_version_match(req_str, &info.version) {
                        Ok(true) => (Severity::Pass, "git URL with consistent version".into()),
                        Ok(false) => (
                            Severity::Info,
                            format!(
                                "version \"{}\" doesn't match local {} (git URL takes precedence)",
                                req_str, info.version
                            ),
                        ),
                        Err(_) => (Severity::Pass, "git URL (version parse skipped)".into()),
                    }
                } else {
                    (Severity::Pass, "git URL".into())
                }
            } else {
                (Severity::Pass, "path replaced with git URL".into())
            }
        }
    };

    DepFinding {
        from_crate: dep.from_crate.clone(),
        to_crate: dep.to_crate.clone(),
        section: dep.section,
        strategy,
        severity,
        message,
    }
}

fn classify_strip_path(
    version_req_str: &str,
    to_crate: &str,
    to_info: Option<&CrateInfo>,
    published: &BTreeMap<String, Option<String>>,
) -> (Severity, String) {
    // Online mode: check crates.io
    if !published.is_empty() {
        match published.get(to_crate) {
            Some(Some(pub_ver)) => {
                match check_version_match(version_req_str, pub_ver) {
                    Ok(true) => (
                        Severity::Pass,
                        format!("version \"{version_req_str}\" resolves on crates.io ({pub_ver})"),
                    ),
                    Ok(false) => (
                        Severity::Error,
                        format!(
                            "version \"{version_req_str}\" doesn't match crates.io {pub_ver}"
                        ),
                    ),
                    Err(_) => (
                        Severity::Warn,
                        format!("couldn't parse version req \"{version_req_str}\""),
                    ),
                }
            }
            Some(None) | None => (
                Severity::Error,
                format!(
                    "'{to_crate}' not published on crates.io — strip_path will fail"
                ),
            ),
        }
    } else {
        // Offline: compare against local version
        if let Some(info) = to_info {
            match check_version_match(version_req_str, &info.version) {
                Ok(true) => (
                    Severity::Pass,
                    format!(
                        "version \"{version_req_str}\" matches local {} (use --online to verify crates.io)",
                        info.version
                    ),
                ),
                Ok(false) => (
                    Severity::Warn,
                    format!(
                        "version \"{version_req_str}\" doesn't match local {}",
                        info.version
                    ),
                ),
                Err(_) => (
                    Severity::Warn,
                    format!("couldn't parse version req \"{version_req_str}\""),
                ),
            }
        } else {
            (
                Severity::Warn,
                format!("'{to_crate}' not found in ecosystem"),
            )
        }
    }
}

fn check_version_match(req_str: &str, version: &str) -> Result<bool, ()> {
    let req = semver::VersionReq::parse(req_str).map_err(|_| ())?;
    let ver = semver::Version::parse(version).map_err(|_| ())?;
    Ok(req.matches(&ver))
}

// ── Sed hack detection ──

fn scan_for_sed_hacks(repo_path: &Path) -> Vec<String> {
    let workflows_dir = repo_path.join(".github/workflows");
    if !workflows_dir.is_dir() {
        return vec![];
    }

    let mut findings = Vec::new();
    let entries = match std::fs::read_dir(&workflows_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("yml") && ext != Some("yaml") {
            continue;
        }
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let filename = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();

        for (i, line) in content.lines().enumerate() {
            let trimmed = line.trim();
            if (trimmed.contains("sed ") || trimmed.starts_with("sed "))
                && (trimmed.contains("Cargo.toml") || trimmed.contains("path"))
            {
                findings.push(format!("{}:{}: {}", filename, i + 1, trimmed));
            }
        }
    }
    findings
}

fn check_uses_ci_prep(repo_path: &Path) -> bool {
    let workflows_dir = repo_path.join(".github/workflows");
    if !workflows_dir.is_dir() {
        return false;
    }

    let entries = match std::fs::read_dir(&workflows_dir) {
        Ok(e) => e,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str());
        if ext != Some("yml") && ext != Some("yaml") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            if content.contains("superwork ci-prep") || content.contains("superwork ci_prep") {
                return true;
            }
        }
    }
    false
}

fn manifest_has_patch_section(manifest_path: &Path) -> bool {
    let content = match std::fs::read_to_string(manifest_path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // Check for workspace root too
    let ws_root = manifest_path
        .parent()
        .map(|p| p.join("Cargo.toml"))
        .filter(|p| p != manifest_path);

    if content.contains("[patch.crates-io]") || content.contains("[patch.\"crates-io\"]") {
        return true;
    }

    // Check workspace root if different from manifest
    if let Some(ws) = ws_root {
        if let Ok(ws_content) = std::fs::read_to_string(&ws) {
            if ws_content.contains("[patch.crates-io]")
                || ws_content.contains("[patch.\"crates-io\"]")
            {
                return true;
            }
        }
    }
    false
}

// ── Report printing ──

fn print_report(report: &LintReport, verbose: bool, online: bool) -> usize {
    let total_repos = report.repos.len();
    println!(
        "=== CI Lint ({total_repos} repos, {} path deps) ===",
        report.total_deps
    );
    println!();

    let mut total_errors = 0;
    let mut total_warnings = 0;
    let mut total_info = 0;
    let mut total_pass = 0;

    for repo in &report.repos {
        let has_visible = repo.findings.iter().any(|f| {
            verbose || f.severity != Severity::Pass
        }) || !repo.sed_hacks.is_empty()
            || (repo.has_ci_config && !repo.uses_ci_prep);

        if !has_visible {
            // Count passes even if not printed
            total_pass += repo.findings.iter().filter(|f| f.severity == Severity::Pass).count();
            continue;
        }

        println!("{}:", repo.repo_dir);

        // Print dep findings
        for f in &repo.findings {
            match f.severity {
                Severity::Pass => total_pass += 1,
                Severity::Info => total_info += 1,
                Severity::Warn => total_warnings += 1,
                Severity::Error => total_errors += 1,
            }

            if !verbose && f.severity == Severity::Pass {
                continue;
            }

            let strategy_label = match f.strategy {
                CiStrategy::GitUrl => "git_url   ",
                CiStrategy::StripPath => "strip_path",
                CiStrategy::Delete => "delete    ",
            };

            if f.to_crate.is_empty() {
                // Repo-level finding (e.g., patch section)
                println!("  {}  {}  {}", f.severity, strategy_label, f.message);
            } else {
                let section_suffix = match f.section {
                    DepSection::DevDependencies => " (dev)",
                    DepSection::BuildDependencies => " (build)",
                    _ => "",
                };
                println!(
                    "  {}  {}  {} -> {}{}  {}",
                    f.severity,
                    strategy_label,
                    f.from_crate,
                    f.to_crate,
                    section_suffix,
                    f.message,
                );
            }
        }

        // Sed hack warnings
        for hack in &repo.sed_hacks {
            total_info += 1;
            println!("  INFO   sed hack   {hack}");
        }

        // Dead ci-prep config warning
        if repo.has_ci_config && !repo.uses_ci_prep {
            total_info += 1;
            println!(
                "  INFO   config     [metadata.superwork.ci] exists but CI doesn't use ci-prep"
            );
        }

        println!();
    }

    // Summary
    println!("=== Summary ===");
    println!(
        "  {total_errors} errors, {total_warnings} warnings, {total_info} info, {total_pass} pass"
    );
    if !online {
        println!("  Run with --online to verify against crates.io");
    }

    total_errors
}

// ── Helpers ──

fn resolve_repo_path(
    ecosystem_root: &Path,
    repo_dir: &str,
    crates: &[&CrateInfo],
) -> std::path::PathBuf {
    // Try to get the repo root from crate info
    if let Some(first) = crates.first() {
        if let Some(ws) = &first.workspace_root {
            if let Some(parent) = ws.parent() {
                return parent.to_path_buf();
            }
        }
        if let Some(parent) = first.manifest_path.parent() {
            return parent.to_path_buf();
        }
    }
    ecosystem_root.join(repo_dir)
}

fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') && !pattern.contains('?') {
        return pattern == text;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return text.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return text.ends_with(suffix);
    }
    if pattern.starts_with('*') && pattern.ends_with('*') {
        let middle = &pattern[1..pattern.len() - 1];
        return text.contains(middle);
    }
    pattern == text
}

// ── Crates.io queries (for --online mode) ──

fn query_published_versions(names: &[String]) -> BTreeMap<String, Option<String>> {
    use std::sync::{Arc, Mutex};
    use std::thread;

    let results: Arc<Mutex<BTreeMap<String, Option<String>>>> =
        Arc::new(Mutex::new(BTreeMap::new()));

    let chunk_size = 16;
    for chunk in names.chunks(chunk_size) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|name| {
                let name = name.clone();
                let results = Arc::clone(&results);
                thread::spawn(move || {
                    let version = query_crate_version(&name);
                    results.lock().unwrap().insert(name, version);
                })
            })
            .collect();

        for h in handles {
            let _ = h.join();
        }
    }

    Arc::try_unwrap(results).unwrap().into_inner().unwrap()
}

fn query_crate_version(crate_name: &str) -> Option<String> {
    let output = std::process::Command::new("cargo")
        .args(["info", crate_name])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        if let Some(v) = line.trim().strip_prefix("version:") {
            let v = v.trim();
            return Some(v.split_whitespace().next().unwrap_or(v).to_string());
        }
    }
    None
}
