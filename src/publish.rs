use crate::config::SuperworkConfig;
use crate::discover::{self, CrateInfo};
use crate::graph;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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
/// If `show_diffs` is true, shows the actual source diff for changed crates.
/// If `src_only` is true, only counts changes under `src/` as real changes.
pub fn run_needs_publish(
    ecosystem_root: &Path,
    config: &SuperworkConfig,
    show_diffs: bool,
    src_only: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;

    let publishable: Vec<&CrateInfo> = eco.crates.values().filter(|c| c.publishable).collect();

    println!(
        "checking {} publishable crates against crates.io (parallel)...",
        publishable.len()
    );

    let owned_orgs: Vec<String> = config.owned_orgs().iter().map(|s| s.to_string()).collect();

    // Query crates.io in parallel
    let crate_names: Vec<String> = publishable.iter().map(|c| c.name.clone()).collect();
    let results = query_crates_io_parallel(&crate_names);

    println!();

    let mut needs_publish: Vec<(String, String, String)> = Vec::new();
    let mut needs_bump: Vec<(String, String)> = Vec::new();
    let mut up_to_date = 0;
    let mut never_published: Vec<(String, String)> = Vec::new();
    let mut external_forks: Vec<(String, String, String)> = Vec::new();

    // Build lookup for CrateInfo by name
    let info_map: BTreeMap<&str, &CrateInfo> =
        publishable.iter().map(|c| (c.name.as_str(), *c)).collect();
    let owned_refs: Vec<&str> = owned_orgs.iter().map(|s| s.as_str()).collect();

    for name in &crate_names {
        let info = info_map[name.as_str()];
        let crates_io = results.get(name).and_then(|r| r.as_ref());

        match crates_io {
            None => {
                never_published.push((name.clone(), info.version.clone()));
            }
            Some((pub_ver, repo_url)) => {
                if !is_owned_repo(repo_url, &owned_refs) {
                    external_forks.push((name.clone(), pub_ver.clone(), repo_url.clone()));
                    continue;
                }

                if pub_ver != &info.version {
                    needs_publish.push((name.clone(), info.version.clone(), pub_ver.clone()));
                } else {
                    let tag = find_version_tag(info);
                    let has_changes = match &tag {
                        Some(t) => {
                            if src_only {
                                has_src_changes_since_tag(info, t)
                            } else {
                                has_source_changes_since_tag(info, t)
                            }
                        }
                        None => true,
                    };

                    if has_changes {
                        needs_bump.push((name.clone(), pub_ver.clone()));
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
            if show_diffs {
                if let Some(info) = info_map.get(name.as_str()) {
                    show_crate_diff(info, Some(published), src_only);
                }
            }
        }
        println!();
    }

    if !needs_bump.is_empty() {
        println!("=== Needs Version Bump (source changed, same version) ===");
        for (name, ver) in &needs_bump {
            println!("  {name} {ver}");
            if show_diffs {
                if let Some(info) = info_map.get(name.as_str()) {
                    let tag = find_version_tag(info);
                    if let Some(t) = &tag {
                        show_crate_diff(info, Some(t), src_only);
                    }
                }
            }
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

    let src_label = if src_only { " (src/ only)" } else { "" };
    println!(
        "=== Summary{src_label} ===\n  {} ready to publish, {} need bump, {} up-to-date, {} never published, {} external forks",
        needs_publish.len(),
        needs_bump.len(),
        up_to_date,
        never_published.len(),
        external_forks.len(),
    );

    let _ = ecosystem_root;
    Ok(())
}

/// Query crates.io for all crate names in parallel.
/// Returns a map of crate_name -> Option<(version, repo_url)>.
fn query_crates_io_parallel(names: &[String]) -> BTreeMap<String, Option<(String, String)>> {
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[allow(clippy::type_complexity)]
    let results: Arc<Mutex<BTreeMap<String, Option<(String, String)>>>> =
        Arc::new(Mutex::new(BTreeMap::new()));

    // Run up to 16 queries at a time
    let chunk_size = 16;
    for chunk in names.chunks(chunk_size) {
        let handles: Vec<_> = chunk
            .iter()
            .map(|name| {
                let name = name.clone();
                let results = Arc::clone(&results);
                thread::spawn(move || {
                    let result = query_crates_io(&name);
                    results.lock().unwrap().insert(name, result);
                })
            })
            .collect();

        for h in handles {
            let _ = h.join();
        }
    }

    Arc::try_unwrap(results).unwrap().into_inner().unwrap()
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
            // Format: "0.6.5" or "0.6.5 (latest 0.7.1)"
            // Take only the first token (the actual version)
            let v = v.trim();
            let v = v.split_whitespace().next().unwrap_or(v);
            version = Some(v.to_string());
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

/// Find the git tag for a crate at a specific version.
/// Tries (in order): {name}-v{ver}, v{ver}, {name}-{ver}
/// Prefers the crate-prefixed tag since repos with multiple crates
/// use prefixed tags (e.g., zensim-v0.2.4 vs zensim-regress-v0.2.3).
fn find_version_tag(info: &CrateInfo) -> Option<String> {
    find_version_tag_for(info, &info.version)
}

fn find_version_tag_for(info: &CrateInfo, version: &str) -> Option<String> {
    let repo_dir = if let Some(ws) = &info.workspace_root {
        ws.parent()?
    } else {
        info.manifest_path.parent()?
    };

    // Prefer crate-prefixed tags (disambiguates in multi-crate repos)
    let candidates = [
        format!("{}-v{version}", info.name),
        format!("v{version}"),
        format!("{}-{version}", info.name),
    ];

    for tag in &candidates {
        let result = Command::new("git")
            .args(["rev-parse", "--verify", &format!("refs/tags/{tag}")])
            .current_dir(repo_dir)
            .output();

        match &result {
            Ok(o) if o.status.success() => return Some(tag.clone()),
            Ok(_) => {} // tag doesn't exist
            Err(e) => {
                eprintln!(
                    "    warn: git rev-parse failed in {}: {e}",
                    repo_dir.display()
                );
            }
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
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true)
}

/// Like has_source_changes_since_tag but only counts changes under src/.
fn has_src_changes_since_tag(info: &CrateInfo, tag: &str) -> bool {
    let (repo_dir, src_path) = crate_paths(info);
    let range = format!("{tag}..HEAD");

    Command::new("git")
        .args(["diff", "--stat", &range, "--", &src_path])
        .current_dir(&repo_dir)
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(true)
}

/// Show a condensed diff summary for a crate since a tag or published version.
fn show_crate_diff(info: &CrateInfo, tag_or_ver: Option<&str>, src_only: bool) {
    let (repo_dir, rel_prefix) = crate_paths(info);

    let tag = tag_or_ver.and_then(|v| {
        // If it looks like a version string, resolve to a tag
        if v.contains('.') {
            find_version_tag_for(info, v)
        } else if v.starts_with('v') || v.contains('-') {
            // Already a tag name
            Some(v.to_string())
        } else {
            find_version_tag(info)
        }
    });

    let Some(tag) = tag else {
        println!("    (no tag found, cannot show diff)");
        return;
    };

    let range = format!("{tag}..HEAD");
    let path_arg = if src_only {
        format!("{rel_prefix}src/")
    } else {
        rel_prefix
    };

    let output = Command::new("git")
        .args(["diff", "--stat", &range, "--", &path_arg])
        .current_dir(&repo_dir)
        .output()
        .ok();

    if let Some(o) = output {
        let stdout = String::from_utf8_lossy(&o.stdout);
        let lines: Vec<&str> = stdout.lines().collect();
        if lines.is_empty() {
            println!("    (no changes{})", if src_only { " in src/" } else { "" });
        } else {
            // Show file changes but limit to 10 lines + summary
            for line in lines.iter().take(10) {
                println!("    {line}");
            }
            if lines.len() > 10 {
                // The last line is usually the summary
                if let Some(last) = lines.last() {
                    println!("    ...");
                    println!("    {last}");
                }
            }
        }
    }
}

/// Get the repo directory and relative path prefix for a crate.
fn crate_paths(info: &CrateInfo) -> (PathBuf, String) {
    let crate_dir = info.manifest_path.parent().unwrap();
    let repo_dir = if let Some(ws) = &info.workspace_root {
        ws.parent().unwrap().to_path_buf()
    } else {
        crate_dir.to_path_buf()
    };

    let rel_prefix = crate_dir
        .strip_prefix(&repo_dir)
        .ok()
        .and_then(|p| {
            let s = p.to_string_lossy();
            if s.is_empty() {
                None
            } else {
                Some(format!("{s}/"))
            }
        })
        .unwrap_or_default();

    (repo_dir, rel_prefix)
}
