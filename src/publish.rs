use crate::config::SuperworkConfig;
use crate::discover::{self, CrateInfo};
use crate::graph;
use std::path::Path;
use std::process::Command;

pub fn run(ecosystem_root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;
    let levels = graph::publish_order(&eco, true)?;

    println!(
        "=== Publish Order ({} publishable crates) ===",
        levels.iter().map(|l| l.len()).sum::<usize>()
    );
    println!();

    for (i, level) in levels.iter().enumerate() {
        if level.is_empty() {
            continue;
        }
        println!("Level {i} ({} crates):", level.len());
        for name in level {
            let version = eco
                .crates
                .get(name)
                .map(|c| c.version.as_str())
                .unwrap_or("?");
            println!("  {name} {version}");
        }
        println!();
    }

    // Check for publishability blockers
    let mut blockers = 0;
    for dep in &eco.deps {
        let from_pub = eco
            .crates
            .get(&dep.from_crate)
            .is_some_and(|c| c.publishable);
        let to_pub = eco.crates.get(&dep.to_crate).is_some_and(|c| c.publishable);
        if from_pub && to_pub && dep.has_path && !dep.has_version {
            if blockers == 0 {
                println!("=== Publish Blockers (path-only deps) ===");
            }
            println!("  {} -> {}: needs version", dep.from_crate, dep.to_crate);
            blockers += 1;
        }
    }

    if blockers > 0 {
        println!();
        println!("{blockers} path-only deps must be dual-specified before publishing.");
    }

    Ok(())
}

/// Check which publishable crates need publishing.
///
/// For each publishable crate:
/// 1. Query crates.io for the published version
/// 2. Compare to local version
/// 3. If versions match, check for source changes since the version tag
pub fn run_needs_publish(ecosystem_root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;

    let publishable: Vec<&CrateInfo> = eco.crates.values().filter(|c| c.publishable).collect();

    println!(
        "checking {} publishable crates against crates.io...",
        publishable.len()
    );
    println!();

    let owned_orgs = config.owned_orgs();

    let mut needs_publish: Vec<(String, String, String)> = Vec::new();
    let mut needs_bump: Vec<(String, String)> = Vec::new();
    let mut up_to_date = 0;
    let mut never_published: Vec<(String, String)> = Vec::new();
    // (name, crates.io version, crates.io repo URL)
    let mut external_forks: Vec<(String, String, String)> = Vec::new();

    for info in &publishable {
        let crates_io = query_crates_io(&info.name);

        match crates_io {
            None => {
                never_published.push((info.name.clone(), info.version.clone()));
            }
            Some((pub_ver, repo_url)) => {
                // Check if this crate is owned by us or is an external fork
                if !is_owned_repo(&repo_url, &owned_orgs) {
                    external_forks.push((info.name.clone(), pub_ver, repo_url));
                    continue;
                }

                if pub_ver != info.version {
                    needs_publish.push((info.name.clone(), info.version.clone(), pub_ver));
                } else {
                    let tag = find_version_tag(info);
                    let has_changes = match &tag {
                        Some(t) => has_source_changes_since_tag(info, t),
                        None => true,
                    };

                    if has_changes {
                        needs_bump.push((info.name.clone(), pub_ver));
                    } else {
                        up_to_date += 1;
                    }
                }
            }
        }
    }

    // Report
    if !needs_publish.is_empty() {
        println!("=== Ready to Publish (version bumped) ===");
        for (name, local, published) in &needs_publish {
            println!("  {name} {published} -> {local}");
        }
        println!();
    }

    if !needs_bump.is_empty() {
        println!("=== Needs Version Bump (source changed, same version) ===");
        for (name, ver) in &needs_bump {
            println!("  {name} {ver} (source changed since publish)");
        }
        println!();
    }

    if !never_published.is_empty() {
        println!("=== Never Published ===");
        for (name, ver) in &never_published {
            println!("  {name} {ver}");
        }
        println!();
    }

    if !external_forks.is_empty() {
        println!("=== External Forks (use crates.io, don't publish) ===");
        for (name, ver, repo) in &external_forks {
            println!("  {name} {ver} ({repo})");
        }
        println!();
    }

    println!(
        "=== Summary ===\n  {} ready to publish, {} need bump, {} up-to-date, {} never published, {} external forks",
        needs_publish.len(),
        needs_bump.len(),
        up_to_date,
        never_published.len(),
        external_forks.len(),
    );

    let _ = ecosystem_root;
    Ok(())
}

/// Query crates.io for version and repository URL.
/// Returns (version, repo_url) or None if not published.
fn query_crates_io(crate_name: &str) -> Option<(String, String)> {
    // Use `cargo info` for full metadata (version + repository)
    let output = Command::new("cargo")
        .args(["info", crate_name])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut version = None;
    let mut repo = String::new();

    for line in stdout.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("version:") {
            version = Some(v.trim().to_string());
        } else if let Some(r) = line.strip_prefix("repository:") {
            repo = r.trim().to_string();
        }
    }

    version.map(|v| (v, repo))
}

/// Check if a crates.io repository URL belongs to one of our owned orgs.
fn is_owned_repo(repo_url: &str, owned_orgs: &[&str]) -> bool {
    if repo_url.is_empty() {
        // No repo URL — can't determine ownership. Assume ours (conservative).
        return true;
    }
    let url_lower = repo_url.to_lowercase();
    for org in owned_orgs {
        let org_lower = org.to_lowercase();
        // Match github.com/org/ or github.com/org.git patterns
        if url_lower.contains(&format!("github.com/{org_lower}/"))
            || url_lower.contains(&format!("github.com/{org_lower}."))
        {
            return true;
        }
    }
    false
}

/// Find the git tag for a crate's current version.
/// Tries: v{version}, {name}-v{version}, {name}-{version}
fn find_version_tag(info: &CrateInfo) -> Option<String> {
    let crate_dir = info.manifest_path.parent()?;
    let repo_dir = if let Some(ws) = &info.workspace_root {
        ws.parent()?
    } else {
        crate_dir
    };

    let candidates = [
        format!("v{}", info.version),
        format!("{}-v{}", info.name, info.version),
        format!("{}-{}", info.name, info.version),
    ];

    for tag in &candidates {
        let ok = Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/tags/{tag}")])
            .current_dir(repo_dir)
            .output()
            .ok()
            .is_some_and(|o| o.status.success());

        if ok {
            return Some(tag.clone());
        }
    }

    None
}

/// Check if source files changed between a tag and HEAD.
fn has_source_changes_since_tag(info: &CrateInfo, tag: &str) -> bool {
    let crate_dir = info.manifest_path.parent().unwrap();
    let repo_dir = if let Some(ws) = &info.workspace_root {
        ws.parent().unwrap()
    } else {
        crate_dir
    };

    // Get relative path of crate within repo
    let rel_path = crate_dir.strip_prefix(repo_dir).ok().and_then(|p| {
        let s = p.to_string_lossy();
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    });

    // git diff --stat <tag>..HEAD -- <path>
    let range = format!("{tag}..HEAD");
    let mut args = vec!["diff", "--stat", range.as_str(), "--"];
    let path_str;
    if let Some(ref rel) = rel_path {
        path_str = format!("{rel}/");
        args.push(&path_str);
    } else {
        args.push(".");
    }

    Command::new("git")
        .args(&args)
        .current_dir(repo_dir)
        .output()
        .ok()
        .map(|o| {
            // If diff output is non-empty, there are changes
            !o.stdout.is_empty()
        })
        .unwrap_or(true) // Assume changed on error
}
