use crate::config::SuperworkConfig;
use crate::discover;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// Run health checks across the ecosystem (edition, license, badges, docs, clutter).
pub fn run(root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;

    // Group crates by repo
    let mut repos: BTreeMap<String, Vec<&discover::CrateInfo>> = BTreeMap::new();
    for info in eco.crates.values() {
        repos.entry(info.repo_dir.clone()).or_default().push(info);
    }

    let mut edition_issues = Vec::new();
    let mut license_issues = Vec::new();
    let mut badge_issues = Vec::new();
    let mut docs_issues = Vec::new();
    let mut clutter_files: Vec<(String, Vec<String>)> = Vec::new();

    for (repo_dir, crates) in &repos {
        let repo_path = root.join(repo_dir);
        if !repo_path.exists() {
            continue;
        }

        // Edition check: all Cargo.toml should use edition 2024
        for ci in crates.iter() {
            if let Ok(content) = std::fs::read_to_string(&ci.manifest_path) {
                let has_2024 = content.lines().any(|l| {
                    let l = l.trim();
                    l == "edition = \"2024\"" || l.contains("edition.workspace = true")
                });
                if !has_2024 {
                    let edition = content
                        .lines()
                        .find(|l| l.trim().starts_with("edition"))
                        .map(|l| l.trim().to_string())
                        .unwrap_or_else(|| "missing".to_string());
                    edition_issues.push((ci.name.clone(), edition));
                }
            }
        }

        // License check: LICENSE file(s) exist
        let has_license = repo_path.join("LICENSE").exists()
            || repo_path.join("LICENSE-AGPL3").exists()
            || repo_path.join("LICENSE-MIT").exists()
            || repo_path.join("LICENSE-APACHE").exists();
        if !has_license {
            license_issues.push(repo_dir.clone());
        }

        // README badge check (publishable crates only)
        let readme_path = repo_path.join("README.md");
        if readme_path.exists() {
            let readme = std::fs::read_to_string(&readme_path).unwrap_or_default();
            let publishable_crates: Vec<&str> = crates
                .iter()
                .filter(|c| c.publishable)
                .map(|c| c.name.as_str())
                .collect();

            if !publishable_crates.is_empty() {
                let mut missing = Vec::new();
                if !readme.contains("img.shields.io") && !readme.contains("badge") {
                    missing.push("no badges at all");
                } else {
                    if !readme.contains("actions/workflow") {
                        missing.push("CI badge");
                    }
                    if !readme.contains("crates.io") && !readme.contains("crates/v/") {
                        missing.push("crates.io badge");
                    }
                    if !readme.contains("docs.rs") && !readme.contains("docsrs") {
                        missing.push("docs.rs badge");
                    }
                }
                if !missing.is_empty() {
                    badge_issues.push((repo_dir.clone(), missing.join(", ")));
                }
            }
        }

        // Docs check: README.md exists and lib.rs has doc comment
        if !readme_path.exists() {
            docs_issues.push((repo_dir.clone(), "no README.md".to_string()));
        }
        for ci in crates.iter() {
            let lib_rs = ci.manifest_path.parent().unwrap().join("src/lib.rs");
            if lib_rs.exists() {
                let content = std::fs::read_to_string(&lib_rs).unwrap_or_default();
                let has_doc = content.starts_with("//!") || content.starts_with("#!");
                if !has_doc {
                    docs_issues.push((
                        ci.name.clone(),
                        "src/lib.rs missing //! doc comment".to_string(),
                    ));
                }
            }
        }

        // Clutter check: AI-generated artifacts
        let clutter_patterns = [
            "SUMMARY.md",
            "PLAN.md",
            "ANALYSIS.md",
            "REVIEW.md",
            "NOTES.md",
            "CHANGES.md",
            "IMPLEMENTATION.md",
            "DESIGN.md",
            "ARCHITECTURE.md",
            "STRATEGY.md",
        ];
        let mut found_clutter = Vec::new();
        for pattern in &clutter_patterns {
            let path = repo_path.join(pattern);
            if path.exists() {
                // Check if it's tracked in git
                let tracked = Command::new("git")
                    .args(["ls-files", pattern])
                    .current_dir(&repo_path)
                    .output()
                    .ok()
                    .is_some_and(|o| !o.stdout.is_empty());
                if !tracked {
                    found_clutter.push(pattern.to_string());
                }
            }
        }
        if !found_clutter.is_empty() {
            clutter_files.push((repo_dir.clone(), found_clutter));
        }
    }

    // Report
    if !edition_issues.is_empty() {
        println!("=== Edition (not 2024) ===");
        for (name, edition) in &edition_issues {
            println!("  {name}: {edition}");
        }
        println!();
    }

    if !license_issues.is_empty() {
        println!("=== Missing LICENSE ===");
        for repo in &license_issues {
            println!("  {repo}");
        }
        println!();
    }

    if !badge_issues.is_empty() {
        println!("=== README Badge Gaps ===");
        for (repo, missing) in &badge_issues {
            println!("  {repo}: {missing}");
        }
        println!();
    }

    if !docs_issues.is_empty() {
        println!("=== Documentation Gaps ===");
        for (name, issue) in &docs_issues {
            println!("  {name}: {issue}");
        }
        println!();
    }

    if !clutter_files.is_empty() {
        println!("=== AI Clutter (untracked) ===");
        for (repo, files) in &clutter_files {
            println!("  {repo}: {}", files.join(", "));
        }
        println!();
    }

    // CI status + open issues — parallel GitHub API calls
    let github_repos: Vec<String> = repos
        .keys()
        .filter_map(|repo_dir| {
            config.github_url_for(repo_dir).and_then(|url| {
                // Extract "org/repo" from https://github.com/org/repo
                url.strip_prefix("https://github.com/")
                    .map(|s| s.trim_end_matches(".git").to_string())
            })
        })
        .collect();

    let (ci_failures, open_issues) = check_github_parallel(&github_repos);

    if !ci_failures.is_empty() {
        println!("=== CI Failures ===");
        for (repo, status) in &ci_failures {
            println!("  {repo}: {status}");
        }
        println!();
    }

    if !open_issues.is_empty() {
        println!("=== Open Issues ===");
        for (repo, issues) in &open_issues {
            for (number, title) in issues {
                println!("  {repo}#{number}: {title}");
            }
        }
        println!();
    }

    let total_issues = edition_issues.len()
        + license_issues.len()
        + badge_issues.len()
        + docs_issues.len()
        + clutter_files.len()
        + ci_failures.len();

    let issue_count: usize = open_issues.values().map(|v| v.len()).sum();

    println!("=== Audit Summary ===");
    println!(
        "  {} edition, {} license, {} badge, {} docs, {} clutter, {} CI failures",
        edition_issues.len(),
        license_issues.len(),
        badge_issues.len(),
        docs_issues.len(),
        clutter_files.len(),
        ci_failures.len(),
    );
    println!(
        "  {} open GitHub issues across {} repos",
        issue_count,
        open_issues.len()
    );

    if total_issues > 0 {
        println!("  {} total audit issues", total_issues);
    } else {
        println!("  all clean");
    }

    Ok(())
}

/// Check CI status and fetch open issues for all repos in parallel.
#[allow(clippy::type_complexity)]
fn check_github_parallel(
    repos: &[String],
) -> (
    Vec<(String, String)>,                // CI failures: (repo, status)
    BTreeMap<String, Vec<(u64, String)>>, // Open issues: repo -> [(number, title)]
) {
    use std::sync::{Arc, Mutex};
    use std::thread;

    let ci_failures: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let open_issues: Arc<Mutex<BTreeMap<String, Vec<(u64, String)>>>> =
        Arc::new(Mutex::new(BTreeMap::new()));

    let chunk_size = 16;
    for chunk in repos.chunks(chunk_size) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|repo| {
                let repo = repo.clone();
                let ci_failures = Arc::clone(&ci_failures);
                let open_issues = Arc::clone(&open_issues);
                thread::spawn(move || {
                    // Check CI status
                    if let Some(status) = check_ci_status(&repo) {
                        if status != "success" && status != "no_runs" {
                            ci_failures.lock().unwrap().push((repo.clone(), status));
                        }
                    }

                    // Fetch open issues
                    let issues = fetch_open_issues(&repo);
                    if !issues.is_empty() {
                        open_issues.lock().unwrap().insert(repo, issues);
                    }
                })
            })
            .collect();

        for h in handles {
            let _ = h.join();
        }
    }

    let ci = Arc::try_unwrap(ci_failures).unwrap().into_inner().unwrap();
    let issues = Arc::try_unwrap(open_issues).unwrap().into_inner().unwrap();
    (ci, issues)
}

/// Check the latest CI run conclusion via `gh run list`.
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

    let conclusion = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if conclusion.is_empty() {
        Some("no_runs".to_string())
    } else {
        Some(conclusion)
    }
}

/// Fetch open issues for a repo via `gh issue list`.
fn fetch_open_issues(repo: &str) -> Vec<(u64, String)> {
    let output = Command::new("gh")
        .args([
            "issue",
            "list",
            "--repo",
            repo,
            "--state",
            "open",
            "--limit",
            "20",
            "--json",
            "number,title",
        ])
        .output()
        .ok();

    let Some(output) = output else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    // Parse JSON: [{"number":1,"title":"..."},...]
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Simple JSON parsing without serde_json
    let mut issues = Vec::new();
    for segment in stdout.split('{').skip(1) {
        let number = segment
            .split("\"number\":")
            .nth(1)
            .and_then(|s| s.split([',', '}'].as_ref()).next())
            .and_then(|s| s.trim().parse::<u64>().ok());

        let title = segment
            .split("\"title\":\"")
            .nth(1)
            .and_then(|s| s.split('"').next())
            .map(|s| s.to_string());

        if let (Some(n), Some(t)) = (number, title) {
            issues.push((n, t));
        }
    }
    issues
}
