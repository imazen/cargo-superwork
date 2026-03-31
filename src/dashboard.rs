use crate::config::SuperworkConfig;
use crate::discover;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;

struct RepoRow {
    repo_dir: String,
    branch: String,
    ci: String,      // "ok", "fail", "no_runs", "?", "-" (no github)
    dirty: usize,
    unpushed: usize,
    worktrees: usize,
    crates: Vec<String>,
}

pub fn run(root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;

    // Group crates by repo dir
    let mut repos: BTreeMap<String, Vec<&discover::CrateInfo>> = BTreeMap::new();
    for info in eco.crates.values() {
        repos.entry(info.repo_dir.clone()).or_default().push(info);
    }

    // Gather git info synchronously (fast)
    let mut rows: Vec<RepoRow> = Vec::new();
    for (repo_dir, crates) in &repos {
        let repo_path = root.join(repo_dir);
        if !repo_path.exists() {
            continue;
        }

        let branch = git_current_branch(&repo_path).unwrap_or_else(|| "?".to_string());
        let dirty = git_status_count(&repo_path);
        let unpushed = git_unpushed_count(&repo_path);
        let worktrees = git_worktree_count(&repo_path).saturating_sub(1); // exclude main

        let mut crate_names: Vec<String> = crates.iter().map(|c| c.name.clone()).collect();
        crate_names.sort();

        rows.push(RepoRow {
            repo_dir: repo_dir.clone(),
            branch,
            ci: "-".to_string(), // filled in below
            dirty,
            unpushed,
            worktrees,
            crates: crate_names,
        });
    }

    // Gather GitHub CI status in parallel
    let github_map: BTreeMap<String, String> = {
        let pairs: Vec<(String, String)> = rows
            .iter()
            .filter_map(|row| {
                config
                    .github_url_for(&row.repo_dir)
                    .and_then(|url| {
                        url.strip_prefix("https://github.com/")
                            .map(|s| s.trim_end_matches(".git").to_string())
                    })
                    .map(|gh| (row.repo_dir.clone(), gh))
            })
            .collect();

        let results: Arc<Mutex<BTreeMap<String, String>>> =
            Arc::new(Mutex::new(BTreeMap::new()));

        for chunk in pairs.chunks(16) {
            let handles: Vec<_> = chunk
                .iter()
                .map(|(repo_dir, gh_repo)| {
                    let repo_dir = repo_dir.clone();
                    let gh_repo = gh_repo.clone();
                    let results = Arc::clone(&results);
                    thread::spawn(move || {
                        let status = check_ci_status(&gh_repo).unwrap_or_else(|| "?".to_string());
                        results.lock().unwrap().insert(repo_dir, status);
                    })
                })
                .collect();
            for h in handles {
                let _ = h.join();
            }
        }

        Arc::try_unwrap(results).unwrap().into_inner().unwrap()
    };

    // Merge CI into rows
    for row in &mut rows {
        if let Some(status) = github_map.get(&row.repo_dir) {
            row.ci = status.clone();
        }
    }

    // Sort: problems first (dirty/unpushed/ci-fail/not-main), then alphabetical
    rows.sort_by(|a, b| {
        let a_score = problem_score(a);
        let b_score = problem_score(b);
        b_score.cmp(&a_score).then(a.repo_dir.cmp(&b.repo_dir))
    });

    // Column widths
    let repo_w = rows.iter().map(|r| r.repo_dir.len()).max().unwrap_or(4).max(4) + 1;
    let branch_w = rows.iter().map(|r| r.branch.len()).max().unwrap_or(6).max(6) + 1;

    // Header
    println!(
        "{:<repo_w$}  {:<branch_w$}  {:<6}  {:<5}  {:<7}  {}",
        "Repo", "Branch", "CI", "Dirty", "Unpush", "Crates"
    );
    println!("{}", "-".repeat(repo_w + branch_w + 36));

    let mut ci_ok = 0;
    let mut ci_fail = 0;
    let mut total_dirty = 0;
    let mut total_unpushed = 0;
    let mut not_main = 0;

    for row in &rows {
        let ci_symbol = match row.ci.as_str() {
            "success" => {
                ci_ok += 1;
                "ok".to_string()
            }
            "-" => "-".to_string(),
            "no_runs" => "?".to_string(),
            s => {
                ci_fail += 1;
                s.to_string()
            }
        };

        let dirty_str = if row.dirty > 0 {
            total_dirty += 1;
            row.dirty.to_string()
        } else {
            "-".to_string()
        };

        let unpush_str = if row.unpushed > 0 {
            total_unpushed += 1;
            row.unpushed.to_string()
        } else {
            "-".to_string()
        };

        let branch_str = if row.branch != "main" && row.branch != "master" {
            not_main += 1;
            format!("{}*", row.branch) // mark non-main with *
        } else {
            row.branch.clone()
        };

        let wt_suffix = if row.worktrees > 0 {
            format!(" +{}wt", row.worktrees)
        } else {
            String::new()
        };

        println!(
            "{:<repo_w$}  {:<branch_w$}  {:<6}  {:<5}  {:<7}  {}{}",
            row.repo_dir,
            branch_str,
            ci_symbol,
            dirty_str,
            unpush_str,
            row.crates.join(", "),
            wt_suffix,
        );
    }

    println!();
    println!(
        "{} repos  |  CI: {} ok, {} fail  |  {} dirty  |  {} unpushed  |  {} not on main",
        rows.len(),
        ci_ok,
        ci_fail,
        total_dirty,
        total_unpushed,
        not_main,
    );

    Ok(())
}

fn problem_score(row: &RepoRow) -> u32 {
    let mut s = 0u32;
    if row.ci != "success" && row.ci != "-" && row.ci != "no_runs" {
        s += 8;
    }
    if row.dirty > 0 {
        s += 4;
    }
    if row.unpushed > 0 {
        s += 2;
    }
    if row.branch != "main" && row.branch != "master" {
        s += 1;
    }
    s
}

fn check_ci_status(repo: &str) -> Option<String> {
    let output = Command::new("gh")
        .args([
            "run",
            "list",
            "--repo",
            repo,
            "--limit",
            "1",
            "--json",
            "conclusion",
            "--jq",
            ".[0].conclusion",
        ])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        Some("no_runs".to_string())
    } else {
        Some(s)
    }
}

fn git_current_branch(repo_path: &Path) -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
}

fn git_status_count(repo_path: &Path) -> usize {
    Command::new("git")
        .args(["status", "--short"])
        .current_dir(repo_path)
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
        })
        .unwrap_or(0)
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
        .is_some_and(|o| o.status.success());
    if !has_remote {
        return 0;
    }
    let range = format!("{remote_ref}..HEAD");
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
}

fn git_worktree_count(repo_path: &Path) -> usize {
    Command::new("git")
        .args(["worktree", "list"])
        .current_dir(repo_path)
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
        })
        .unwrap_or(1)
}
