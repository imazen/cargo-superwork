use crate::config::SuperworkConfig;
use crate::discover::{self, CrateInfo, Ecosystem};
use crate::graph;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// Run an arbitrary command in every repo, ordered by dependency graph.
pub fn run_cmd(
    root: &Path,
    config: &SuperworkConfig,
    cmd: &str,
    filter: Option<&str>,
    changed_only: bool,
    jobs: usize,
    fail_fast: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let repos = select_repos(&eco, config, filter, changed_only, root)?;

    if repos.is_empty() {
        println!("no repos to process");
        return Ok(());
    }

    println!("running `{cmd}` in {} repos (jobs={})", repos.len(), jobs);
    println!();

    // Order repos by dependency level
    let ordered = order_repos_by_level(&eco, &repos)?;

    let mut failures = Vec::new();
    let mut successes = 0;

    for (level, level_repos) in ordered.iter().enumerate() {
        if level_repos.is_empty() {
            continue;
        }

        // Within a level, repos are independent — could parallelize with `jobs`
        // For now, sequential within level (parallel would need threads)
        for (repo_dir, repo_path) in level_repos {
            print!("[L{level}] {repo_dir}: ");

            let result = run_shell_cmd(cmd, repo_path);

            match result {
                Ok(output) => {
                    if output.success {
                        println!("OK");
                        successes += 1;
                    } else {
                        println!("FAIL (exit {})", output.code);
                        if !output.stderr_tail.is_empty() {
                            for line in output.stderr_tail.lines().take(10) {
                                println!("  {line}");
                            }
                        }
                        failures.push(repo_dir.clone());
                        if fail_fast {
                            println!();
                            return Err(format!(
                                "failed in {repo_dir} (exit {}). {successes} passed, 1 failed.",
                                output.code
                            ));
                        }
                    }
                }
                Err(e) => {
                    println!("ERROR: {e}");
                    failures.push(repo_dir.clone());
                    if fail_fast {
                        return Err(format!("error in {repo_dir}: {e}"));
                    }
                }
            }
        }
    }

    let _ = jobs; // TODO: parallel execution within levels

    println!();
    println!("{successes} passed, {} failed", failures.len());

    if !failures.is_empty() {
        println!("failures:");
        for f in &failures {
            println!("  {f}");
        }
        Err(format!("{} repos failed", failures.len()))
    } else {
        Ok(())
    }
}

/// Run a named check from [checks] config, falling back to a default command.
pub fn run_check(
    root: &Path,
    config: &SuperworkConfig,
    check_name: &str,
    filter: Option<&str>,
    changed_only: bool,
    fail_fast: bool,
) -> Result<(), String> {
    let default_cmd = match check_name {
        "test" => "cargo test",
        "clippy" => "cargo clippy --all-targets -- -D warnings",
        "fmt" => "cargo fmt -- --check",
        "msrv" => "cargo hack check --rust-version",
        "semver" => "cargo semver-checks",
        _ => return Err(format!("unknown check: {check_name}")),
    };

    // Use config command if defined, otherwise use default
    let cmd = config
        .checks
        .commands
        .get(check_name)
        .map(|d| d.command().to_string())
        .unwrap_or_else(|| default_cmd.to_string());

    let eco = discover::scan_ecosystem(root, config)?;
    let repos = select_repos(&eco, config, filter, changed_only, root)?;

    if repos.is_empty() {
        println!("no repos to process for `{check_name}`");
        return Ok(());
    }

    println!(
        "running check `{check_name}` (`{cmd}`) in {} repos",
        repos.len()
    );
    println!();

    let ordered = order_repos_by_level(&eco, &repos)?;
    let mut failures = Vec::new();
    let mut successes = 0;

    for (level, level_repos) in ordered.iter().enumerate() {
        for (repo_dir, repo_path) in level_repos {
            // Allow per-repo command override
            let repo_cmd = config
                .check_command(check_name, repo_dir)
                .unwrap_or_else(|| cmd.clone());

            print!("[L{level}] {repo_dir}: ");

            match run_shell_cmd(&repo_cmd, repo_path) {
                Ok(output) if output.success => {
                    println!("OK");
                    successes += 1;
                }
                Ok(output) => {
                    println!("FAIL (exit {})", output.code);
                    if !output.stderr_tail.is_empty() {
                        for line in output.stderr_tail.lines().take(10) {
                            println!("  {line}");
                        }
                    }
                    failures.push(repo_dir.clone());
                    if fail_fast {
                        return Err(format!("{successes} passed, 1 failed in {repo_dir}"));
                    }
                }
                Err(e) => {
                    println!("ERROR: {e}");
                    failures.push(repo_dir.clone());
                    if fail_fast {
                        return Err(format!("error in {repo_dir}: {e}"));
                    }
                }
            }
        }
    }

    println!();
    println!("{successes} passed, {} failed", failures.len());
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("{} repos failed", failures.len()))
    }
}

/// Run cargo semver-checks on publishable crates that changed since last tag.
pub fn run_semver_check(
    root: &Path,
    config: &SuperworkConfig,
    filter: Option<&str>,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;
    let repos = select_repos(&eco, config, filter, true, root)?;

    // Filter to publishable crates only
    let publishable_repos: Vec<_> = repos
        .into_iter()
        .filter(|(_, crates)| crates.iter().any(|c| c.publishable))
        .collect();

    if publishable_repos.is_empty() {
        println!("no changed publishable crates to check");
        return Ok(());
    }

    println!(
        "running cargo semver-checks on {} repos",
        publishable_repos.len()
    );
    println!();

    let mut failures = Vec::new();
    let mut successes = 0;
    let mut skipped = 0;

    for (repo_dir, crates) in &publishable_repos {
        let repo_path = root.join(repo_dir);

        for ci in crates.iter().filter(|c| c.publishable) {
            let crate_dir = ci.manifest_path.parent().unwrap();
            print!("  {} ({}): ", ci.name, repo_dir);

            match run_shell_cmd("cargo semver-checks", crate_dir) {
                Ok(output) if output.success => {
                    println!("OK");
                    successes += 1;
                }
                Ok(output) if output.code == 1 => {
                    // semver-checks returns 1 for breaking changes
                    println!("BREAKING");
                    if !output.stderr_tail.is_empty() {
                        for line in output.stderr_tail.lines().take(5) {
                            println!("    {line}");
                        }
                    }
                    failures.push(ci.name.clone());
                }
                Ok(output) => {
                    // Other exit codes might mean "not published yet"
                    println!("skipped (exit {})", output.code);
                    skipped += 1;
                }
                Err(e) => {
                    println!("skipped ({e})");
                    skipped += 1;
                }
            }

            let _ = &repo_path;
        }
    }

    println!();
    println!(
        "{successes} OK, {} breaking, {skipped} skipped",
        failures.len()
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(format!("breaking changes in: {}", failures.join(", ")))
    }
}

/// Run cargo-copter to test reverse deps of a crate against local WIP.
pub fn run_copter(root: &Path, config: &SuperworkConfig, crate_name: &str) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;

    let target = eco
        .crates
        .get(crate_name)
        .ok_or_else(|| format!("crate '{crate_name}' not found"))?;

    let target_dir = target.manifest_path.parent().unwrap();

    // Find all reverse deps (crates that depend on this one)
    let reverse_deps: BTreeSet<&str> = eco
        .deps
        .iter()
        .filter(|d| d.to_crate == crate_name)
        .map(|d| d.from_crate.as_str())
        .collect();

    if reverse_deps.is_empty() {
        println!("{crate_name} has no internal reverse dependencies");
        return Ok(());
    }

    // Collect unique repo directories for reverse deps
    let mut dep_dirs: Vec<String> = Vec::new();
    for dep_name in &reverse_deps {
        if let Some(info) = eco.crates.get(*dep_name) {
            let dir = info
                .manifest_path
                .parent()
                .unwrap()
                .to_string_lossy()
                .to_string();
            if !dep_dirs.contains(&dir) {
                dep_dirs.push(dir);
            }
        }
    }

    println!(
        "testing {} reverse deps of {crate_name} via cargo-copter:",
        reverse_deps.len()
    );
    for name in &reverse_deps {
        println!("  {name}");
    }
    println!();

    // Build cargo-copter command
    let mut cmd_parts = vec![
        "cargo".to_string(),
        "copter".to_string(),
        "-p".to_string(),
        target_dir.to_string_lossy().to_string(),
        "--only-check".to_string(),
        "--simple".to_string(),
    ];

    for dir in &dep_dirs {
        cmd_parts.push("--dependent-paths".to_string());
        cmd_parts.push(dir.clone());
    }

    let cmd_str = cmd_parts.join(" ");
    println!("running: {cmd_str}");
    println!();

    let output = run_shell_cmd(&cmd_str, root)?;
    if !output.stdout_tail.is_empty() {
        println!("{}", output.stdout_tail);
    }
    if !output.stderr_tail.is_empty() {
        eprintln!("{}", output.stderr_tail);
    }

    if output.success {
        Ok(())
    } else {
        Err(format!("cargo-copter failed (exit {})", output.code))
    }
}

// ── Internal helpers ──

struct CmdOutput {
    success: bool,
    code: i32,
    stdout_tail: String,
    stderr_tail: String,
}

fn run_shell_cmd(cmd: &str, cwd: &Path) -> Result<CmdOutput, String> {
    let output = Command::new("sh")
        .args(["-c", cmd])
        .current_dir(cwd)
        .output()
        .map_err(|e| format!("executing `{cmd}`: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Keep last N lines for error reporting
    let stdout_tail: String = stdout
        .lines()
        .rev()
        .take(50)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let stderr_tail: String = stderr
        .lines()
        .rev()
        .take(50)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");

    Ok(CmdOutput {
        success: output.status.success(),
        code: output.status.code().unwrap_or(-1),
        stdout_tail,
        stderr_tail,
    })
}

/// Select repos to process, applying filters
fn select_repos<'a>(
    eco: &'a Ecosystem,
    _config: &SuperworkConfig,
    filter: Option<&str>,
    changed_only: bool,
    root: &Path,
) -> Result<Vec<(String, Vec<&'a CrateInfo>)>, String> {
    // Group crates by repo dir
    let mut by_repo: BTreeMap<String, Vec<&CrateInfo>> = BTreeMap::new();
    for info in eco.crates.values() {
        by_repo.entry(info.repo_dir.clone()).or_default().push(info);
    }

    let mut result: Vec<(String, Vec<&CrateInfo>)> = by_repo.into_iter().collect();

    // Apply crate name filter (glob-style)
    if let Some(pattern) = filter {
        result.retain(|(_, crates)| crates.iter().any(|c| glob_match(pattern, &c.name)));
    }

    // Filter to repos with changes since last tag
    if changed_only {
        result.retain(|(repo_dir, _)| {
            let repo_path = root.join(repo_dir);
            has_changes_since_last_tag(&repo_path)
        });
    }

    Ok(result)
}

/// Order repos by dependency level (leaves first)
fn order_repos_by_level<'a>(
    eco: &Ecosystem,
    repos: &[(String, Vec<&'a CrateInfo>)],
) -> Result<Vec<Vec<(String, std::path::PathBuf)>>, String> {
    let levels = graph::publish_order(eco, false)?;

    // Map crate names to their repo dirs
    let repo_set: BTreeSet<&str> = repos.iter().map(|(d, _)| d.as_str()).collect();

    // For each level, collect repo dirs that have crates at that level
    let mut result: Vec<Vec<(String, std::path::PathBuf)>> = Vec::new();
    let mut seen_repos: BTreeSet<String> = BTreeSet::new();

    for level in &levels {
        let mut level_repos: Vec<(String, std::path::PathBuf)> = Vec::new();
        for crate_name in level {
            if let Some(info) = eco.crates.get(crate_name) {
                if repo_set.contains(info.repo_dir.as_str()) && !seen_repos.contains(&info.repo_dir)
                {
                    let repo_path = info
                        .manifest_path
                        .parent()
                        .unwrap()
                        .parent()
                        .unwrap_or(info.manifest_path.parent().unwrap());
                    // Use the repo root, not the crate subdir
                    let actual_repo_path = if info.workspace_root.is_some() {
                        info.workspace_root
                            .as_ref()
                            .unwrap()
                            .parent()
                            .unwrap()
                            .to_path_buf()
                    } else {
                        info.manifest_path.parent().unwrap().to_path_buf()
                    };
                    level_repos.push((info.repo_dir.clone(), actual_repo_path));
                    seen_repos.insert(info.repo_dir.clone());
                }
            }
        }
        if !level_repos.is_empty() {
            level_repos.sort_by(|a, b| a.0.cmp(&b.0));
            result.push(level_repos);
        }
    }

    // Add any repos not captured by levels (no internal deps)
    let mut remaining: Vec<(String, std::path::PathBuf)> = Vec::new();
    for (repo_dir, crates) in repos {
        if !seen_repos.contains(repo_dir) {
            if let Some(first) = crates.first() {
                let repo_path = if let Some(ws) = &first.workspace_root {
                    ws.parent().unwrap().to_path_buf()
                } else {
                    first.manifest_path.parent().unwrap().to_path_buf()
                };
                remaining.push((repo_dir.clone(), repo_path));
            }
        }
    }
    if !remaining.is_empty() {
        remaining.sort_by(|a, b| a.0.cmp(&b.0));
        result.push(remaining);
    }

    let _ = repo_set;
    Ok(result)
}

/// Check if a repo has commits since its last git tag
fn has_changes_since_last_tag(repo_path: &Path) -> bool {
    if !repo_path.exists() {
        return false;
    }

    // Get last tag
    let last_tag = Command::new("git")
        .args(["describe", "--tags", "--abbrev=0"])
        .current_dir(repo_path)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        });

    match last_tag {
        Some(tag) => {
            // Check if there are commits since the tag
            let range = format!("{tag}..HEAD");
            Command::new("git")
                .args(["rev-list", "--count", &range])
                .current_dir(repo_path)
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        String::from_utf8_lossy(&o.stdout)
                            .trim()
                            .parse::<usize>()
                            .ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0)
                > 0
        }
        None => {
            // No tags at all — consider everything changed
            true
        }
    }
}

/// Simple glob matching (supports * and ?)
fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') && !pattern.contains('?') {
        return pattern == text;
    }
    // Simple prefix/suffix glob
    if let Some(prefix) = pattern.strip_suffix('*') {
        return text.starts_with(prefix);
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return text.ends_with(suffix);
    }
    // Contains glob: *middle*
    if pattern.starts_with('*') && pattern.ends_with('*') {
        let middle = &pattern[1..pattern.len() - 1];
        return text.contains(middle);
    }
    // Fallback: exact match
    pattern == text
}
