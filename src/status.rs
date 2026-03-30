use crate::config::EcosystemConfig;
use crate::discover;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

pub fn run(ecosystem_root: &Path, config: &EcosystemConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;

    // Group crates by repo dir
    let mut repos: BTreeMap<&str, Vec<&discover::CrateInfo>> = BTreeMap::new();
    for info in eco.crates.values() {
        repos.entry(&info.repo_dir).or_default().push(info);
    }

    println!("=== Repo Status ({} repos) ===", repos.len());
    println!();

    let mut dirty_count = 0;
    let mut unpushed_count = 0;

    for (repo_dir, crates) in &repos {
        let repo_path = ecosystem_root.join(repo_dir);
        if !repo_path.join(".git").exists() && !repo_path.join(".git").is_file() {
            // Could be a worktree (.git is a file) or not a git repo
            // Check if it's a symlink to another repo
            let canonical = repo_path.canonicalize().unwrap_or(repo_path.clone());
            if !canonical.join(".git").exists() && !canonical.join(".git").is_file() {
                println!("{repo_dir}: (not a git repo)");
                continue;
            }
        }

        let dirty = git_status_short(&repo_path);
        let unpushed = git_unpushed_count(&repo_path);

        let crate_names: Vec<&str> = crates.iter().map(|c| c.name.as_str()).collect();
        let status_parts: Vec<String> = [
            if dirty > 0 {
                dirty_count += 1;
                Some(format!("{dirty} dirty"))
            } else {
                None
            },
            if unpushed > 0 {
                unpushed_count += 1;
                Some(format!("{unpushed} unpushed"))
            } else {
                None
            },
        ]
        .into_iter()
        .flatten()
        .collect();

        let status_str = if status_parts.is_empty() {
            "clean".to_string()
        } else {
            status_parts.join(", ")
        };

        println!("{repo_dir} [{status_str}]: {}", crate_names.join(", "));
    }

    // Version mismatches
    println!();
    println!("=== Version Mismatches ===");
    let mut mismatches = 0;
    for dep in &eco.deps {
        if let (Some(ver_req), Some(info)) = (&dep.version_value, eco.crates.get(&dep.to_crate)) {
            if let (Ok(req), Ok(ver)) = (
                semver::VersionReq::parse(ver_req),
                semver::Version::parse(&info.version),
            ) {
                if !req.matches(&ver) {
                    println!(
                        "  {} -> {} requires \"{ver_req}\", actual is {}",
                        dep.from_crate, dep.to_crate, info.version
                    );
                    mismatches += 1;
                }
            }
        }
    }
    if mismatches == 0 {
        println!("  (none)");
    }

    println!();
    println!("=== Summary ===");
    println!(
        "  {} repos, {} dirty, {} with unpushed commits",
        repos.len(),
        dirty_count,
        unpushed_count
    );
    println!("  {} version mismatches", mismatches);

    Ok(())
}

fn git_status_short(repo_path: &Path) -> usize {
    Command::new("git")
        .args(["status", "--short"])
        .current_dir(repo_path)
        .output()
        .ok()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .count()
        })
        .unwrap_or(0)
}

fn git_unpushed_count(repo_path: &Path) -> usize {
    // Get current branch
    let branch = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        });

    let Some(branch) = branch else { return 0 };

    // Check if remote tracking branch exists
    let remote_ref = format!("origin/{branch}");
    let has_remote = Command::new("git")
        .args(["rev-parse", "--verify", &remote_ref])
        .current_dir(repo_path)
        .output()
        .ok()
        .is_some_and(|out| out.status.success());

    if !has_remote {
        return 0;
    }

    // Count commits ahead of remote
    let range = format!("{remote_ref}..HEAD");
    Command::new("git")
        .args(["rev-list", "--count", &range])
        .current_dir(repo_path)
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                String::from_utf8_lossy(&out.stdout)
                    .trim()
                    .parse::<usize>()
                    .ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
}
