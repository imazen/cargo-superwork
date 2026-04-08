//! Clone dependency repos as siblings for CI builds.
//!
//! `ci-clone` clones ecosystem repos that the CWD project depends on.
//! `--add-paths` adds `path = "..."` keys to version-only deps so the
//! cloned source is used instead of crates.io.
//! `--recursive` expands transitively: each cloned repo's deps are also
//! cloned and pathed, until the full closure is resolved.

use crate::config::SuperworkConfig;
use crate::manifest;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn run(
    _ecosystem_root: &Path,
    config: &SuperworkConfig,
    add_paths: bool,
    recursive: bool,
    dry_run: bool,
) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| format!("cannot determine CWD: {e}"))?;
    let parent = cwd
        .parent()
        .ok_or_else(|| "CWD has no parent directory".to_string())?;

    // Build crate_name → (repo_dir_name, git_url) from patch_repos + repo overrides.
    let crate_to_repo = build_crate_repo_map(config);

    // Phase 0: delete deps listed in inline CI config (private/unavailable deps).
    let delete_deps = read_delete_list(&cwd);
    if !delete_deps.is_empty() && !dry_run {
        delete_deps_from_project(&cwd, &delete_deps)?;
        println!("  deleted deps: {}", delete_deps.join(", "));
    }

    // Phase 1: find which sibling dirs we need from the CWD project's deps.
    let mut needed_dirs: BTreeSet<String> = BTreeSet::new();
    collect_needed_dirs(&cwd, &crate_to_repo, &mut needed_dirs)?;

    // Phase 2: clone, then recurse.
    let label = if dry_run { "[dry-run] " } else { "" };
    let mut cloned = 0;
    let mut existed = 0;
    let mut pathed_files = 0;

    // Iterative expansion: clone → scan new repos → clone their deps → repeat.
    let mut processed_dirs: BTreeSet<String> = BTreeSet::new();
    // The CWD project's dir is already "processed" (we don't clone ourselves).
    if let Some(cwd_name) = cwd.file_name().and_then(|s| s.to_str()) {
        processed_dirs.insert(cwd_name.to_string());
    }

    loop {
        let dirs_to_process: Vec<String> =
            needed_dirs.difference(&processed_dirs).cloned().collect();

        if dirs_to_process.is_empty() {
            break;
        }

        for dir in &dirs_to_process {
            processed_dirs.insert(dir.clone());
            let target = parent.join(dir);

            if target.exists() {
                println!("{label}  exists: {dir}");
                existed += 1;
            } else {
                let url = find_repo_url(dir, config);
                let Some(url) = url else {
                    eprintln!("  warning: no git URL for '{dir}', skipping");
                    continue;
                };

                println!("{label}  clone:  {dir} ← {url}");

                if !dry_run {
                    let status = Command::new("git")
                        .args(["clone", "--depth", "1", &url, &target.to_string_lossy()])
                        .status()
                        .map_err(|e| format!("git clone {url}: {e}"))?;

                    if !status.success() {
                        eprintln!("  ERROR: git clone failed for {dir}");
                        continue;
                    }
                }
                cloned += 1;
            }

            // If --recursive, scan the cloned repo for its own deps and add to needed_dirs.
            if recursive && target.exists() {
                collect_needed_dirs(&target, &crate_to_repo, &mut needed_dirs)?;
            }
        }
    }

    // Phase 3: add path overrides AFTER all repos are cloned.
    // This ensures all sibling dirs exist when computing paths.
    if add_paths && !dry_run {
        // Add paths to the CWD project
        for dir in &processed_dirs {
            let n = add_path_overrides(&cwd, dir, &crate_to_repo)?;
            pathed_files += n;
        }

        // Add paths within each cloned repo, pointing to other cloned siblings
        for dir in &processed_dirs {
            let target = parent.join(dir);
            if target.exists() && target != cwd {
                let n = add_path_overrides_to_repo(&target, &crate_to_repo, &processed_dirs)?;
                pathed_files += n;
            }
        }

        // Also wildcard ALL existing path dep versions (not just newly added).
        // Version mismatches between Cargo.toml and cloned repos cause errors
        // even with path deps — cargo still checks version compatibility.
        wildcard_all_path_dep_versions(&cwd)?;
        for dir in &processed_dirs {
            let target = parent.join(dir);
            if target.exists() && target != cwd {
                wildcard_all_path_dep_versions(&target)?;
            }
        }

        println!("{label}  pathed {pathed_files} manifests");
    }

    println!(
        "{label}{cloned} cloned, {existed} existed ({} total repos)",
        processed_dirs.len() - 1 // subtract CWD itself
    );
    Ok(())
}

/// Build a map: crate_name → (sibling_dir_name, git_url).
/// The dir name comes from the repo URL's last segment, which is the
/// actual repo name (matching the directory it clones into).
fn build_crate_repo_map(config: &SuperworkConfig) -> BTreeMap<String, (String, String)> {
    let mut map = BTreeMap::new();

    // First pass: build URL → dir_name mapping from [[repo]] overrides
    let mut url_to_dir: BTreeMap<String, String> = BTreeMap::new();
    for r in &config.repo {
        if let Some(gh) = &r.github {
            let url = format!("https://github.com/{gh}");
            let dir = r.dir.strip_prefix("../").unwrap_or(&r.dir).to_string();
            url_to_dir.insert(url, dir);
        }
    }

    for (crate_name, url) in &config.ci.patch_repos {
        // Use [[repo]] override dir if available, otherwise derive from URL
        let dir = if let Some(d) = url_to_dir.get(url) {
            d.clone()
        } else {
            url.strip_prefix("https://github.com/")
                .and_then(|s| s.split('/').nth(1))
                .unwrap_or(crate_name)
                .to_string()
        };
        map.insert(crate_name.clone(), (dir, url.clone()));
    }
    map
}

/// Scan Cargo.toml files in `repo_dir` for internal deps and add their
/// sibling dirs to `needed_dirs`.
fn collect_needed_dirs(
    repo_dir: &Path,
    crate_to_repo: &BTreeMap<String, (String, String)>,
    needed_dirs: &mut BTreeSet<String>,
) -> Result<(), String> {
    // Find all Cargo.toml files in this repo
    let manifests = find_manifests(repo_dir);

    for manifest_path in &manifests {
        let content = std::fs::read_to_string(manifest_path).unwrap_or_default();
        let doc: toml::Value = match toml::from_str(&content) {
            Ok(d) => d,
            Err(_) => continue,
        };

        // Scan [dependencies], [dev-dependencies], [build-dependencies]
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(deps) = doc.get(section).and_then(|d| d.as_table()) else {
                continue;
            };
            for (dep_name, dep_value) in deps {
                let actual_name = dep_value
                    .as_table()
                    .and_then(|t| t.get("package"))
                    .and_then(|p| p.as_str())
                    .unwrap_or(dep_name);

                // If this dep has a path already, extract the sibling dir
                if let Some(path) = dep_value
                    .as_table()
                    .and_then(|t| t.get("path"))
                    .and_then(|p| p.as_str())
                {
                    if let Some(dir) = extract_sibling_dir(path) {
                        needed_dirs.insert(dir);
                    }
                }
                // If no path but the crate is in our ecosystem, we'll need its repo
                else if let Some((dir, _)) = crate_to_repo.get(actual_name) {
                    needed_dirs.insert(dir.clone());
                }
            }
        }

        // Also scan [workspace.dependencies]
        if let Some(ws_deps) = doc
            .get("workspace")
            .and_then(|w| w.as_table())
            .and_then(|w| w.get("dependencies"))
            .and_then(|d| d.as_table())
        {
            for (dep_name, dep_value) in ws_deps {
                let actual_name = dep_value
                    .as_table()
                    .and_then(|t| t.get("package"))
                    .and_then(|p| p.as_str())
                    .unwrap_or(dep_name);

                if let Some(path) = dep_value
                    .as_table()
                    .and_then(|t| t.get("path"))
                    .and_then(|p| p.as_str())
                {
                    if let Some(dir) = extract_sibling_dir(path) {
                        needed_dirs.insert(dir);
                    }
                } else if let Some((dir, _)) = crate_to_repo.get(actual_name) {
                    needed_dirs.insert(dir.clone());
                }
            }
        }
    }

    Ok(())
}

/// Add `path = "../{dir}/..."` to deps in the CWD project that reference
/// crates from the given sibling dir.
fn add_path_overrides(
    project_dir: &Path,
    sibling_dir: &str,
    crate_to_repo: &BTreeMap<String, (String, String)>,
) -> Result<usize, String> {
    let manifests = find_manifests(project_dir);
    let mut modified = 0;

    // Find which crates live in this sibling dir
    let crates_in_dir: Vec<&str> = crate_to_repo
        .iter()
        .filter(|(_, (dir, _))| dir == sibling_dir)
        .map(|(name, _)| name.as_str())
        .collect();

    for manifest_path in &manifests {
        // Read the raw TOML to check which deps already have paths
        let raw_content = std::fs::read_to_string(manifest_path).unwrap_or_default();
        let raw_doc: toml::Value =
            toml::from_str(&raw_content).unwrap_or(toml::Value::Table(Default::default()));

        let (_, mut doc) = manifest::read_manifest(manifest_path)?;
        let mut changes = 0;

        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            for crate_name in &crates_in_dir {
                // Skip deps that already have a path — don't overwrite correct paths
                let already_has_path = raw_doc
                    .get(section)
                    .and_then(|s| s.as_table())
                    .and_then(|t| t.get(*crate_name))
                    .and_then(|d| d.as_table())
                    .and_then(|t| t.get("path"))
                    .is_some();
                if already_has_path {
                    continue;
                }

                let path = compute_dep_path(
                    manifest_path,
                    project_dir,
                    sibling_dir,
                    crate_name,
                    crate_to_repo,
                );
                if let Some(path) = path {
                    if manifest::set_dep_path(&mut doc, section, crate_name, &path) {
                        // Also set version to "*" so any version matches the cloned code
                        manifest::set_dep_version(&mut doc, section, crate_name, "*");
                        changes += 1;
                    }
                }
            }
        }

        if changes > 0 {
            manifest::write_manifest(manifest_path, &doc, false)?;
            modified += 1;
        }
    }

    Ok(modified)
}

/// Add path overrides to a cloned repo's manifests, pointing to other cloned siblings.
fn add_path_overrides_to_repo(
    repo_dir: &Path,
    crate_to_repo: &BTreeMap<String, (String, String)>,
    available_dirs: &BTreeSet<String>,
) -> Result<usize, String> {
    let manifests = find_manifests(repo_dir);
    let mut modified = 0;

    for manifest_path in &manifests {
        let raw_content = std::fs::read_to_string(manifest_path).unwrap_or_default();
        let raw_doc: toml::Value =
            toml::from_str(&raw_content).unwrap_or(toml::Value::Table(Default::default()));

        let (_, mut doc) = manifest::read_manifest(manifest_path)?;
        let mut changes = 0;

        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(deps) = raw_doc.get(section).and_then(|d| d.as_table()) else {
                continue;
            };
            let dep_names: Vec<String> = deps.keys().map(|k| k.to_string()).collect();

            for dep_name in &dep_names {
                // Skip deps that already have paths
                let already_has_path = deps
                    .get(dep_name)
                    .and_then(|d| d.as_table())
                    .and_then(|t| t.get("path"))
                    .is_some();
                if already_has_path {
                    continue;
                }

                if let Some((dir, _)) = crate_to_repo.get(dep_name.as_str()) {
                    if available_dirs.contains(dir) {
                        let path =
                            compute_dep_path(manifest_path, repo_dir, dir, dep_name, crate_to_repo);
                        if let Some(path) = path {
                            if manifest::set_dep_path(&mut doc, section, dep_name, &path) {
                                manifest::set_dep_version(&mut doc, section, dep_name, "*");
                                changes += 1;
                            }
                        }
                    }
                }
            }
        }

        // Also handle [workspace.dependencies]
        if let Some(ws) = doc.get("workspace").and_then(|w| w.as_table()) {
            if let Some(ws_deps) = ws.get("dependencies").and_then(|d| d.as_table()) {
                let dep_names: Vec<String> = ws_deps.iter().map(|(k, _)| k.to_string()).collect();
                for dep_name in &dep_names {
                    if let Some((dir, _)) = crate_to_repo.get(dep_name.as_str()) {
                        if available_dirs.contains(dir) {
                            let path = compute_dep_path(
                                manifest_path,
                                repo_dir,
                                dir,
                                dep_name,
                                crate_to_repo,
                            );
                            if let Some(ref path) = path {
                                // For workspace deps, navigate manually
                                if let Some(ws) =
                                    doc.get_mut("workspace").and_then(|w| w.as_table_mut())
                                {
                                    if let Some(ws_deps) = ws
                                        .get_mut("dependencies")
                                        .and_then(|d| d.as_table_like_mut())
                                    {
                                        if let Some(dep) = ws_deps.get_mut(dep_name.as_str()) {
                                            if let Some(tbl) = dep.as_inline_table_mut() {
                                                if !tbl.contains_key("path") {
                                                    tbl.insert("path", path.as_str().into());
                                                    tbl.insert("version", "*".into());
                                                    changes += 1;
                                                }
                                            } else if let Some(tbl) = dep.as_table_mut() {
                                                if !tbl.contains_key("path") {
                                                    tbl.insert(
                                                        "path",
                                                        toml_edit::value(path.as_str()),
                                                    );
                                                    tbl.insert("version", toml_edit::value("*"));
                                                    changes += 1;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if changes > 0 {
            manifest::write_manifest(manifest_path, &doc, false)?;
            modified += 1;
        }
    }

    Ok(modified)
}

/// Compute the relative path from a manifest to a crate in a sibling dir.
/// For workspace repos, finds the actual crate directory by scanning
/// the workspace members for a matching [package].name.
fn compute_dep_path(
    manifest_path: &Path,
    _repo_dir: &Path,
    sibling_dir: &str,
    crate_name: &str,
    _crate_to_repo: &BTreeMap<String, (String, String)>,
) -> Option<String> {
    let manifest_dir = manifest_path.parent()?;
    let parent = manifest_dir.parent()?;
    let sibling_path = parent.join(sibling_dir);

    if !sibling_path.exists() {
        return None;
    }

    // Check if the root Cargo.toml IS the crate (not a virtual workspace)
    let root_toml = sibling_path.join("Cargo.toml");
    if root_toml.exists() {
        if let Ok(content) = std::fs::read_to_string(&root_toml) {
            if let Ok(doc) = content.parse::<toml::Value>() {
                // Direct package match at root
                if doc
                    .get("package")
                    .and_then(|p| p.get("name"))
                    .and_then(|n| n.as_str())
                    == Some(crate_name)
                {
                    return Some(pathdiff(manifest_dir, &sibling_path));
                }

                // Workspace — search members for the crate
                if let Some(members) = doc
                    .get("workspace")
                    .and_then(|w| w.as_table())
                    .and_then(|w| w.get("members"))
                    .and_then(|m| m.as_array())
                {
                    for member in members {
                        if let Some(member_str) = member.as_str() {
                            // Handle globs
                            if member_str.contains('*') {
                                let prefix = member_str.split('*').next().unwrap_or("");
                                let glob_dir = sibling_path.join(prefix);
                                if let Ok(entries) = std::fs::read_dir(&glob_dir) {
                                    for entry in entries.flatten() {
                                        if let Some(p) = check_member_name(
                                            &entry.path(),
                                            crate_name,
                                            manifest_dir,
                                        ) {
                                            return Some(p);
                                        }
                                    }
                                }
                            } else {
                                let member_path = sibling_path.join(member_str);
                                if let Some(p) =
                                    check_member_name(&member_path, crate_name, manifest_dir)
                                {
                                    return Some(p);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // No matching crate found — don't add a wrong path
    None
}

/// Check if a workspace member directory contains a crate with the given name.
fn check_member_name(member_dir: &Path, crate_name: &str, from_dir: &Path) -> Option<String> {
    let toml_path = member_dir.join("Cargo.toml");
    let content = std::fs::read_to_string(&toml_path).ok()?;
    let doc: toml::Value = toml::from_str(&content).ok()?;
    let name = doc
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())?;
    if name == crate_name {
        Some(pathdiff(from_dir, member_dir))
    } else {
        None
    }
}

fn pathdiff(from: &Path, to: &Path) -> String {
    // Count how many levels up from `from` to common ancestor, then down to `to`
    let from = from.canonicalize().unwrap_or_else(|_| from.to_path_buf());
    let to = to.canonicalize().unwrap_or_else(|_| to.to_path_buf());

    let from_parts: Vec<_> = from.components().collect();
    let to_parts: Vec<_> = to.components().collect();

    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let ups = from_parts.len() - common;
    let mut result = PathBuf::new();
    for _ in 0..ups {
        result.push("..");
    }
    for part in &to_parts[common..] {
        result.push(part);
    }
    result.to_string_lossy().to_string()
}

/// Read the delete list from [package.metadata.superwork.ci] or [workspace.metadata.superwork.ci].
fn read_delete_list(project_dir: &Path) -> Vec<String> {
    let root_toml = project_dir.join("Cargo.toml");
    let content = std::fs::read_to_string(&root_toml).unwrap_or_default();
    let doc: toml::Value = match toml::from_str(&content) {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };

    // Check [package.metadata.superwork.ci.delete] and [workspace.metadata.superwork.ci.delete]
    for base in ["package", "workspace"] {
        if let Some(delete) = doc
            .get(base)
            .and_then(|p| p.get("metadata"))
            .and_then(|m| m.get("superwork"))
            .and_then(|s| s.get("ci"))
            .and_then(|c| c.get("delete"))
            .and_then(|d| d.as_array())
        {
            return delete
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect();
        }
    }

    Vec::new()
}

/// Delete deps from all manifests in the project, including feature references.
fn delete_deps_from_project(project_dir: &Path, deps_to_delete: &[String]) -> Result<(), String> {
    let manifests = find_manifests(project_dir);

    for manifest_path in &manifests {
        let (_, mut doc) = manifest::read_manifest(manifest_path)?;
        let mut changes = 0;

        for dep_name in deps_to_delete {
            for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if manifest::delete_dep(&mut doc, section, dep_name) {
                    changes += 1;
                }
            }
            changes += manifest::strip_dep_from_features(&mut doc, dep_name);
        }

        if changes > 0 {
            manifest::write_manifest(manifest_path, &doc, false)?;
        }
    }

    Ok(())
}

/// Set version = "*" on all deps that have a path key.
/// This prevents version mismatch errors between Cargo.toml declarations
/// and actual cloned repo versions.
fn wildcard_all_path_dep_versions(repo_dir: &Path) -> Result<(), String> {
    let manifests = find_manifests(repo_dir);

    for manifest_path in &manifests {
        let raw = std::fs::read_to_string(manifest_path).unwrap_or_default();
        let raw_doc: toml::Value =
            toml::from_str(&raw).unwrap_or(toml::Value::Table(Default::default()));
        let (_, mut doc) = manifest::read_manifest(manifest_path)?;
        let mut changes = 0;

        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let Some(deps) = raw_doc.get(section).and_then(|d| d.as_table()) else {
                continue;
            };
            for (name, val) in deps {
                let has_path = val.as_table().and_then(|t| t.get("path")).is_some();
                let has_version = val.as_table().and_then(|t| t.get("version")).is_some()
                    || val.as_str().is_some();
                if has_path && has_version {
                    if manifest::set_dep_version(&mut doc, section, name, "*") {
                        changes += 1;
                    }
                }
            }
        }

        // Also handle [workspace.dependencies]
        if let Some(ws_deps) = raw_doc
            .get("workspace")
            .and_then(|w| w.as_table())
            .and_then(|w| w.get("dependencies"))
            .and_then(|d| d.as_table())
        {
            for (name, val) in ws_deps {
                let has_path = val.as_table().and_then(|t| t.get("path")).is_some();
                let has_version = val.as_table().and_then(|t| t.get("version")).is_some();
                if has_path && has_version {
                    // Navigate workspace.dependencies manually for toml_edit
                    if let Some(ws) = doc.get_mut("workspace").and_then(|w| w.as_table_mut()) {
                        if let Some(ws_deps) = ws
                            .get_mut("dependencies")
                            .and_then(|d| d.as_table_like_mut())
                        {
                            if let Some(dep) = ws_deps.get_mut(name) {
                                if let Some(tbl) = dep.as_inline_table_mut() {
                                    if tbl.get("version").and_then(|v| v.as_str()) != Some("*") {
                                        tbl.insert("version", "*".into());
                                        changes += 1;
                                    }
                                } else if let Some(tbl) = dep.as_table_mut() {
                                    if tbl.get("version").and_then(|v| v.as_str()) != Some("*") {
                                        tbl.insert("version", toml_edit::value("*"));
                                        changes += 1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        if changes > 0 {
            manifest::write_manifest(manifest_path, &doc, false)?;
        }
    }

    Ok(())
}

/// Find all Cargo.toml files in a repo (workspace root + members).
fn find_manifests(repo_dir: &Path) -> Vec<PathBuf> {
    let mut manifests = Vec::new();
    let root_toml = repo_dir.join("Cargo.toml");
    if !root_toml.exists() {
        return manifests;
    }
    manifests.push(root_toml.clone());

    // Check for workspace members
    if let Ok(content) = std::fs::read_to_string(&root_toml) {
        if let Ok(doc) = content.parse::<toml::Value>() {
            if let Some(members) = doc
                .get("workspace")
                .and_then(|w| w.as_table())
                .and_then(|w| w.get("members"))
                .and_then(|m| m.as_array())
            {
                for member in members {
                    if let Some(member_str) = member.as_str() {
                        // Handle simple paths (not globs)
                        let member_toml = repo_dir.join(member_str).join("Cargo.toml");
                        if member_toml.exists() {
                            manifests.push(member_toml);
                        }
                        // Handle globs like "crates/*"
                        if member_str.contains('*') {
                            let prefix = member_str.split('*').next().unwrap_or("");
                            let glob_dir = repo_dir.join(prefix);
                            if let Ok(entries) = std::fs::read_dir(&glob_dir) {
                                for entry in entries.flatten() {
                                    let p = entry.path().join("Cargo.toml");
                                    if p.exists() {
                                        manifests.push(p);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    manifests
}

/// Extract the sibling directory name from a relative path.
fn extract_sibling_dir(path: &str) -> Option<String> {
    let parts: Vec<&str> = path.split('/').collect();
    // "../zenresize" → "zenresize"
    // "../zenjpeg/zenjpeg" → "zenjpeg"
    // "../../imageflow/imageflow_core" → "imageflow"
    if parts.len() >= 2 && parts[0] == ".." && parts[1] != ".." {
        Some(parts[1].to_string())
    } else if parts.len() >= 3 && parts[0] == ".." && parts[1] == ".." {
        Some(parts[2].to_string())
    } else {
        None
    }
}

/// Find the git URL for a sibling directory name.
fn find_repo_url(dir: &str, config: &SuperworkConfig) -> Option<String> {
    // Check [[repo]] overrides
    for r in &config.repo {
        let r_dir = r.dir.strip_prefix("../").unwrap_or(&r.dir);
        if r_dir == dir {
            if r.no_remote {
                return None;
            }
            if let Some(gh) = &r.github {
                return Some(format!("https://github.com/{gh}"));
            }
        }
    }

    // Check patch_repos — find a URL whose repo name matches
    for url in config.ci.patch_repos.values() {
        let repo_name = url
            .strip_prefix("https://github.com/")
            .and_then(|s| s.split('/').nth(1));
        if repo_name == Some(dir) {
            return Some(url.clone());
        }
    }

    // Fallback: convention
    Some(format!(
        "https://github.com/{}/{}",
        config.meta().default_github_org,
        dir
    ))
}
