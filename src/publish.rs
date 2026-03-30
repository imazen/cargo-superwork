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

    // (name, local_version, published_version)
    let mut needs_publish: Vec<(String, String, String)> = Vec::new();
    // (name, published_version)
    let mut needs_bump: Vec<(String, String)> = Vec::new();
    let mut up_to_date = 0;
    let mut never_published: Vec<(String, String)> = Vec::new();

    for info in &publishable {
        let published_version = query_crates_io_version(&info.name);

        match published_version {
            None => {
                never_published.push((info.name.clone(), info.version.clone()));
            }
            Some(pub_ver) if pub_ver != info.version => {
                needs_publish.push((info.name.clone(), info.version.clone(), pub_ver));
            }
            Some(pub_ver) => {
                // Same version — check if source changed since tag
                let tag = find_version_tag(info);
                let has_changes = match &tag {
                    Some(t) => has_source_changes_since_tag(info, t),
                    None => true, // No tag found, assume changed
                };

                if has_changes {
                    needs_bump.push((info.name.clone(), pub_ver));
                } else {
                    up_to_date += 1;
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

    println!(
        "=== Summary ===\n  {} ready to publish, {} need bump, {} up-to-date, {} never published",
        needs_publish.len(),
        needs_bump.len(),
        up_to_date,
        never_published.len()
    );

    let _ = ecosystem_root;
    Ok(())
}

/// Query crates.io for the latest published version of a crate.
/// Returns None if the crate has never been published.
fn query_crates_io_version(crate_name: &str) -> Option<String> {
    // Use `cargo search` — scan results for exact name match.
    // Output lines: `crate_name = "X.Y.Z"    # description`
    let output = Command::new("cargo")
        .args(["search", crate_name, "--limit", "25"])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    // The exact match must be `crate_name = "` at start of line (after trim)
    // to distinguish `app` from `app-config`, `appy`, etc.
    let exact_prefix = format!("{crate_name} = \"");

    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(&exact_prefix) {
            if let Some(end) = rest.find('"') {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
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
