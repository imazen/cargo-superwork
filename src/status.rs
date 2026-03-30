use crate::config::SuperworkConfig;
use crate::discover;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

pub fn run(ecosystem_root: &Path, config: &SuperworkConfig) -> Result<(), String> {
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
    let mut not_on_main = Vec::new();
    let mut no_remote = Vec::new();

    for (repo_dir, crates) in &repos {
        let repo_path = ecosystem_root.join(repo_dir);
        if !is_git_repo(&repo_path) {
            println!("{repo_dir}: (not a git repo)");
            continue;
        }

        let dirty = git_status_short(&repo_path);
        let unpushed = git_unpushed_count(&repo_path);
        let branch = git_current_branch(&repo_path);
        let has_remote = git_has_remote(&repo_path);

        let crate_names: Vec<&str> = crates.iter().map(|c| c.name.as_str()).collect();

        let mut parts: Vec<String> = Vec::new();

        // Branch info
        let branch_str = branch.as_deref().unwrap_or("???");
        let is_main = matches!(branch_str, "main" | "master");
        if !is_main {
            not_on_main.push((*repo_dir, branch_str.to_string()));
            parts.push(format!("branch={branch_str}"));
        }

        if dirty > 0 {
            dirty_count += 1;
            parts.push(format!("{dirty} dirty"));
        }
        if unpushed > 0 {
            unpushed_count += 1;
            parts.push(format!("{unpushed} unpushed"));
        }
        if !has_remote {
            no_remote.push(*repo_dir);
            parts.push("no remote".to_string());
        }

        let status_str = if parts.is_empty() {
            "clean".to_string()
        } else {
            parts.join(", ")
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

    // Not on main
    if !not_on_main.is_empty() {
        println!();
        println!("=== Not on main/master ===");
        for (repo, branch) in &not_on_main {
            println!("  {repo}: {branch}");
        }
    }

    // No remote
    if !no_remote.is_empty() {
        println!();
        println!("=== No remote configured ===");
        for repo in &no_remote {
            println!("  {repo}");
        }
    }

    println!();
    println!("=== Summary ===");
    println!(
        "  {} repos, {} dirty, {} unpushed, {} not on main, {} no remote",
        repos.len(),
        dirty_count,
        unpushed_count,
        not_on_main.len(),
        no_remote.len(),
    );
    println!("  {} version mismatches", mismatches);

    Ok(())
}

/// Inventory all worktrees across all repos.
pub fn run_worktrees(ecosystem_root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;

    // Collect unique repo dirs
    let mut repo_dirs: Vec<String> = eco.crates.values().map(|c| c.repo_dir.clone()).collect();
    repo_dirs.sort();
    repo_dirs.dedup();

    let mut total_worktrees = 0;
    let mut dirty_worktrees = 0;
    let mut unmerged_worktrees = 0;

    for repo_dir in &repo_dirs {
        let repo_path = ecosystem_root.join(repo_dir);
        if !is_git_repo(&repo_path) {
            continue;
        }

        let worktrees = list_worktrees(&repo_path);
        if worktrees.len() <= 1 {
            // Only the main worktree — skip
            continue;
        }

        let main_branch = git_default_branch(&repo_path);

        println!("=== {repo_dir} ({} worktrees) ===", worktrees.len());

        for wt in &worktrees {
            total_worktrees += 1;
            let mut flags = Vec::new();

            // Check if it's the main worktree
            let is_main_wt = wt.path == repo_path.to_string_lossy();
            if is_main_wt {
                flags.push("main");
            }

            // Dirty check
            let wt_path = Path::new(&wt.path);
            if wt_path.exists() {
                let dirty = git_status_short(wt_path);
                if dirty > 0 {
                    dirty_worktrees += 1;
                    flags.push("DIRTY");
                }
            } else {
                flags.push("MISSING");
            }

            // Merged check: is this branch's HEAD reachable from main?
            if !is_main_wt {
                let merged = is_branch_merged(&repo_path, &wt.head, &main_branch);
                if !merged {
                    unmerged_worktrees += 1;
                    flags.push("unmerged");
                } else {
                    flags.push("merged");
                }
            }

            // Pushed check
            if let Some(ref branch) = wt.branch {
                let branch_name = branch.strip_prefix("refs/heads/").unwrap_or(branch);
                let pushed = is_branch_pushed(&repo_path, branch_name);
                if !pushed {
                    flags.push("not pushed");
                }

                let flag_str = if flags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", flags.join(", "))
                };

                println!("  {branch_name}{flag_str}");
                println!("    {}", wt.path);
            } else {
                let flag_str = if flags.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", flags.join(", "))
                };
                println!("  (detached {}){flag_str}", &wt.head[..8]);
                println!("    {}", wt.path);
            }
        }
        println!();
    }

    println!("=== Summary ===");
    println!(
        "  {} worktrees across {} repos",
        total_worktrees,
        repo_dirs.len()
    );
    println!("  {dirty_worktrees} dirty, {unmerged_worktrees} unmerged");

    Ok(())
}

// ── Git helpers ──

struct Worktree {
    path: String,
    head: String,
    branch: Option<String>,
}

fn list_worktrees(repo_path: &Path) -> Vec<Worktree> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(repo_path)
        .output()
        .ok();

    let Some(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut worktrees = Vec::new();
    let mut current_path = None;
    let mut current_head = None;
    let mut current_branch = None;

    for line in stdout.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            // Save previous worktree if any
            if let (Some(path), Some(head)) = (current_path.take(), current_head.take()) {
                worktrees.push(Worktree {
                    path,
                    head,
                    branch: current_branch.take(),
                });
            }
            current_path = Some(p.to_string());
        } else if let Some(h) = line.strip_prefix("HEAD ") {
            current_head = Some(h.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            current_branch = Some(b.to_string());
        } else if line == "detached" {
            current_branch = None;
        }
    }
    // Last entry
    if let (Some(path), Some(head)) = (current_path, current_head) {
        worktrees.push(Worktree {
            path,
            head,
            branch: current_branch,
        });
    }

    worktrees
}

fn is_git_repo(path: &Path) -> bool {
    if path.join(".git").exists() || path.join(".git").is_file() {
        return true;
    }
    let canonical = path.canonicalize().unwrap_or(path.to_path_buf());
    canonical.join(".git").exists() || canonical.join(".git").is_file()
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

fn git_current_branch(repo_path: &Path) -> Option<String> {
    Command::new("git")
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
        })
}

fn git_has_remote(repo_path: &Path) -> bool {
    Command::new("git")
        .args(["remote"])
        .current_dir(repo_path)
        .output()
        .ok()
        .is_some_and(|out| {
            out.status.success() && !String::from_utf8_lossy(&out.stdout).trim().is_empty()
        })
}

fn git_default_branch(repo_path: &Path) -> String {
    // Try origin/HEAD, fall back to "main", then "master"
    let symbolic = Command::new("git")
        .args(["symbolic-ref", "refs/remotes/origin/HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                s.strip_prefix("refs/remotes/origin/")
                    .map(|b| b.to_string())
            } else {
                None
            }
        });

    if let Some(branch) = symbolic {
        return branch;
    }

    // Check if main or master exists
    for candidate in ["main", "master"] {
        let ok = Command::new("git")
            .args(["rev-parse", "--verify", candidate])
            .current_dir(repo_path)
            .output()
            .ok()
            .is_some_and(|o| o.status.success());
        if ok {
            return candidate.to_string();
        }
    }

    "main".to_string()
}

fn git_unpushed_count(repo_path: &Path) -> usize {
    let branch = match git_current_branch(repo_path) {
        Some(b) => b,
        None => return 0,
    };

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

/// Check if a commit is reachable from (merged into) a branch.
fn is_branch_merged(repo_path: &Path, commit: &str, into_branch: &str) -> bool {
    Command::new("git")
        .args(["merge-base", "--is-ancestor", commit, into_branch])
        .current_dir(repo_path)
        .output()
        .ok()
        .is_some_and(|o| o.status.success())
}

/// Check if a branch has been pushed to origin.
fn is_branch_pushed(repo_path: &Path, branch: &str) -> bool {
    let remote_ref = format!("origin/{branch}");
    let remote_exists = Command::new("git")
        .args(["rev-parse", "--verify", &remote_ref])
        .current_dir(repo_path)
        .output()
        .ok()
        .is_some_and(|o| o.status.success());

    if !remote_exists {
        return false;
    }

    // Check if local and remote are at the same commit
    let local = Command::new("git")
        .args(["rev-parse", branch])
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

    let remote = Command::new("git")
        .args(["rev-parse", &remote_ref])
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

    // If remote exists but is behind local, not fully pushed
    // Just check that local is ancestor-of-or-equal-to remote
    match (local, remote) {
        (Some(l), Some(r)) => l == r,
        _ => false,
    }
}
