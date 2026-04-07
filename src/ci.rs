use crate::config::{CiStrategy, MergedCiOverride, SuperworkConfig};
use crate::discover::{self, DepSection, Ecosystem};
use crate::manifest;
use std::collections::BTreeMap;
use std::path::Path;

/// Run ci-prep: transform Cargo.toml files for CI
pub fn run(
    ecosystem_root: &Path,
    config: &SuperworkConfig,
    filter_crate: Option<&str>,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem_with_cwd(ecosystem_root, config)?;

    // Group deps by manifest file (the Cargo.toml being modified)
    let mut by_manifest: BTreeMap<&Path, Vec<&discover::InternalDep>> = BTreeMap::new();
    for dep in &eco.deps {
        by_manifest.entry(&dep.manifest_path).or_default().push(dep);
    }

    // Determine which crates to process
    let crates_to_process: Vec<&str> = if let Some(name) = filter_crate {
        vec![name]
    } else {
        // Process all crates that have CI overrides, plus all crates with internal path deps
        let mut names: Vec<&str> = eco
            .deps
            .iter()
            .filter(|d| d.has_path)
            .map(|d| d.from_crate.as_str())
            .collect();
        names.sort();
        names.dedup();
        names
    };

    let mut files_modified = 0;
    let mut changes = 0;
    let mut processed_ws_roots = std::collections::BTreeSet::<std::path::PathBuf>::new();

    // Process each crate
    for crate_name in &crates_to_process {
        let crate_info = match eco.crates.get(*crate_name) {
            Some(c) => c,
            None => {
                if filter_crate.is_some() {
                    return Err(format!("crate '{crate_name}' not found in ecosystem"));
                }
                continue;
            }
        };

        // Apply CI overrides to the crate's own manifest
        let manifest_path = &crate_info.manifest_path;
        let n = apply_ci_transforms(manifest_path, crate_name, &eco, config, dry_run)?;
        if n > 0 {
            files_modified += 1;
            changes += n;
        }

        // If this crate is in a workspace, apply workspace-level overrides
        // and transform workspace.dependencies path deps to git URLs
        let inline_ci_ref = crate_info.inline_ci.as_ref();
        if let Some(ws_root) = &crate_info.workspace_root {
            // Only process each workspace root once
            if !processed_ws_roots.contains(ws_root) {
                processed_ws_roots.insert(ws_root.clone());

                let merged = config.ci_override_for(crate_name, inline_ci_ref);
                let n_overrides = if let Some(ref m) = merged {
                    apply_workspace_transforms(ws_root, m, dry_run)?
                } else {
                    0
                };

                let n_deps = transform_workspace_deps(ws_root, &eco, config, dry_run)?;

                let n = n_overrides + n_deps;
                if n > 0 {
                    files_modified += 1;
                    changes += n;
                }
            }
        } else {
            // Not a workspace member — check for workspace.dependencies in the crate's own manifest
            // (crate is both a package and a workspace root)
            let ws_path = crate_info.manifest_path.clone();
            if !processed_ws_roots.contains(&ws_path) {
                processed_ws_roots.insert(ws_path.clone());

                if let Some(ref merged) = config.ci_override_for(crate_name, inline_ci_ref) {
                    let n = apply_workspace_transforms(&ws_path, merged, dry_run)?;
                    if n > 0 {
                        files_modified += 1;
                        changes += n;
                    }
                }

                let n = transform_workspace_deps(&ws_path, &eco, config, dry_run)?;
                if n > 0 {
                    files_modified += 1;
                    changes += n;
                }
            }
        }

        // Apply per-member-crate overrides (for workspaces like zenjpeg)
        if let Some(merged) = config.ci_override_for(crate_name, inline_ci_ref) {
            for (member, member_deps) in merged.delete_crate_deps() {
                let member_manifest = find_member_manifest(ecosystem_root, crate_info, member)?;
                if let Some(member_path) = member_manifest {
                    let n = apply_member_transforms(
                        &member_path,
                        member,
                        member_deps,
                        &merged,
                        dry_run,
                    )?;
                    if n > 0 {
                        files_modified += 1;
                        changes += n;
                    }
                }
            }
        }
    }

    // Also process any workspace roots not yet handled — they may have
    // [workspace.dependencies] path entries that need transformation even if
    // no member crate had cross-repo path deps in its own manifest.
    for info in eco.crates.values() {
        let ws_root = if let Some(ref ws) = info.workspace_root {
            ws.clone()
        } else if info.manifest_path.parent().unwrap().join("Cargo.toml") == info.manifest_path {
            // Crate IS the workspace root — check if it has workspace.dependencies
            info.manifest_path.clone()
        } else {
            continue;
        };

        if processed_ws_roots.contains(&ws_root) {
            continue;
        }
        processed_ws_roots.insert(ws_root.clone());

        let n = transform_workspace_deps(&ws_root, &eco, config, dry_run)?;
        if n > 0 {
            files_modified += 1;
            changes += n;
        }
    }

    if dry_run {
        println!("[dry-run] would modify {files_modified} files ({changes} changes)");
    } else {
        println!("modified {files_modified} files ({changes} changes)");
    }

    Ok(())
}

/// Apply CI transforms to a single manifest file
fn apply_ci_transforms(
    manifest_path: &Path,
    crate_name: &str,
    eco: &Ecosystem,
    config: &SuperworkConfig,
    dry_run: bool,
) -> Result<usize, String> {
    let (_, mut doc) = manifest::read_manifest(manifest_path)?;
    let mut changes = 0;

    // Find all internal deps in this manifest
    let deps: Vec<_> = eco
        .deps
        .iter()
        .filter(|d| d.from_crate == crate_name && d.manifest_path == manifest_path && d.has_path)
        .collect();

    let inline_ci = eco
        .crates
        .get(crate_name)
        .and_then(|c| c.inline_ci.as_ref());

    // Determine the source crate's repo dir for same-repo detection
    let from_repo = eco.crates.get(crate_name).map(|c| c.repo_dir.as_str());

    for dep in &deps {
        // Skip intra-repo deps — they resolve naturally within the checkout
        let to_repo = eco
            .crates
            .get(dep.to_crate.as_str())
            .map(|c| c.repo_dir.as_str());
        if from_repo.is_some() && from_repo == to_repo {
            continue;
        }

        let strategy = config.ci_strategy_for(crate_name, &dep.to_crate, inline_ci);
        let section = dep_section_key(dep.section);

        match strategy {
            CiStrategy::GitUrl => {
                // Replace path with git URL
                let git_url =
                    match dep_git_url(&dep.to_crate, dep.path_value.as_deref(), eco, config) {
                        Some(url) => url,
                        None => {
                            eprintln!(
                                "  warning: no git URL for {} (dep of {}), skipping",
                                dep.to_crate, crate_name
                            );
                            continue;
                        }
                    };
                if manifest::replace_path_with_git(&mut doc, section, &dep.to_crate, &git_url) {
                    changes += 1;
                }
            }
            CiStrategy::StripPath => {
                // Remove path, keep version (requires dual-spec)
                if !dep.has_version {
                    eprintln!(
                        "  warning: strip_path on {}->{} but no version specified",
                        crate_name, dep.to_crate
                    );
                }
                if manifest::remove_dep_path(&mut doc, section, &dep.to_crate) {
                    changes += 1;
                }
            }
            CiStrategy::Delete => {
                if manifest::delete_dep(&mut doc, section, &dep.to_crate) {
                    changes += 1;
                }
                // Also strip dep:name references from [features]
                changes += manifest::strip_dep_from_features(&mut doc, &dep.to_crate);
                // Strip using dep_key too (handles package renames)
                if dep.dep_key != dep.to_crate {
                    changes += manifest::strip_dep_from_features(&mut doc, &dep.dep_key);
                }
            }
        }
    }

    // Delete sections (e.g., patch.crates-io) — check inline + central
    if let Some(merged) = config.ci_override_for(crate_name, inline_ci) {
        for section in merged.delete_sections() {
            if manifest::delete_section(&mut doc, section) {
                changes += 1;
            }
        }
    }

    if changes > 0 {
        let wrote = manifest::write_manifest(manifest_path, &doc, dry_run)?;
        if wrote {
            let label = if dry_run { "[dry-run] " } else { "" };
            println!("{label}{}: {changes} changes", manifest_path.display());
        }
    }

    Ok(changes)
}

/// Apply workspace-level transforms (member removal, workspace dep removal, section deletion)
fn apply_workspace_transforms(
    ws_root: &Path,
    ovr: &MergedCiOverride<'_>,
    dry_run: bool,
) -> Result<usize, String> {
    let (_, mut doc) = manifest::read_manifest(ws_root)?;
    let mut changes = 0;

    for member in ovr.delete_members() {
        if manifest::remove_workspace_member(&mut doc, member) {
            changes += 1;
        }
    }

    for dep in ovr.delete_workspace_deps() {
        if manifest::remove_workspace_dep(&mut doc, dep) {
            changes += 1;
        }
    }

    // Also delete sections from the workspace root (e.g., patch.crates-io)
    for section in ovr.delete_sections() {
        if manifest::delete_section(&mut doc, section) {
            changes += 1;
        }
    }

    if changes > 0 {
        let wrote = manifest::write_manifest(ws_root, &doc, dry_run)?;
        if wrote {
            let label = if dry_run { "[dry-run] " } else { "" };
            println!("{label}{}: {changes} workspace changes", ws_root.display());
        }
    }

    Ok(changes)
}

/// Transform [workspace.dependencies] path entries to git URLs.
/// Only transforms deps that reference crates in OTHER repos (not intra-workspace paths).
fn transform_workspace_deps(
    ws_root: &Path,
    eco: &Ecosystem,
    config: &SuperworkConfig,
    dry_run: bool,
) -> Result<usize, String> {
    let ws_dir = ws_root.parent().unwrap();

    if !ws_root.exists() {
        return Ok(0);
    }

    // Read the workspace Cargo.toml to find [workspace.dependencies] with path entries
    let content = std::fs::read_to_string(ws_root)
        .map_err(|e| format!("reading {}: {e}", ws_root.display()))?;
    let parsed: toml::Value =
        toml::from_str(&content).map_err(|e| format!("parsing {}: {e}", ws_root.display()))?;

    let ws_deps = match parsed
        .get("workspace")
        .and_then(|w| w.as_table())
        .and_then(|w| w.get("dependencies"))
        .and_then(|d| d.as_table())
    {
        Some(t) => t,
        None => return Ok(0),
    };

    // Find workspace deps that have path entries pointing outside this repo
    let mut to_transform: Vec<(String, String)> = Vec::new(); // (dep_key, git_url)

    for (dep_name, dep_value) in ws_deps {
        let path = dep_value
            .as_table()
            .and_then(|t| t.get("path"))
            .and_then(|p| p.as_str());
        let Some(path_str) = path else {
            continue;
        };

        // Check if path points outside the workspace directory.
        // Use lexical normalization (resolve .. without requiring path to exist)
        // because in CI the target directory may not be checked out.
        let resolved = normalize_path(&ws_dir.join(path_str));
        let ws_normalized = normalize_path(ws_dir);
        if resolved.starts_with(&ws_normalized) {
            continue; // Intra-workspace path, skip
        }

        // This is an external dep — resolve the actual crate name and get git URL
        let actual_name = dep_value
            .as_table()
            .and_then(|t| t.get("package"))
            .and_then(|p| p.as_str())
            .unwrap_or(dep_name);

        let git_url = dep_git_url(actual_name, Some(path_str), eco, config);

        let Some(git_url) = git_url else {
            continue;
        };

        to_transform.push((dep_name.clone(), git_url));
    }

    if to_transform.is_empty() {
        return Ok(0);
    }

    // Apply transforms using toml_edit
    let (_, mut doc) = manifest::read_manifest(ws_root)?;
    let mut changes = 0;

    for (dep_name, git_url) in &to_transform {
        // Navigate to workspace.dependencies.{dep_name} and replace path with git
        if let Some(ws) = doc.get_mut("workspace").and_then(|w| w.as_table_mut()) {
            if let Some(ws_deps) = ws
                .get_mut("dependencies")
                .and_then(|d| d.as_table_like_mut())
            {
                if let Some(dep) = ws_deps.get_mut(dep_name) {
                    let replaced = if let Some(tbl) = dep.as_inline_table_mut() {
                        tbl.remove("path");
                        tbl.insert("git", git_url.as_str().into());
                        true
                    } else if let Some(tbl) = dep.as_table_mut() {
                        tbl.remove("path");
                        tbl.insert("git", toml_edit::value(git_url.as_str()));
                        true
                    } else {
                        false
                    };
                    if replaced {
                        changes += 1;
                    }
                }
            }
        }
    }

    if changes > 0 {
        // Also delete [patch.crates-io] if it has path entries
        if manifest::delete_section(&mut doc, "patch.crates-io") {
            changes += 1;
        }

        let wrote = manifest::write_manifest(ws_root, &doc, dry_run)?;
        if wrote {
            let label = if dry_run { "[dry-run] " } else { "" };
            let dep_names: Vec<&str> = to_transform.iter().map(|(n, _)| n.as_str()).collect();
            println!(
                "{label}{}: {} workspace deps → git ({})",
                ws_root.display(),
                changes,
                dep_names.join(", ")
            );
        }
    }

    Ok(changes)
}

/// Apply per-member-crate transforms (dep deletion, feature stripping, key blanking)
fn apply_member_transforms(
    member_manifest: &Path,
    member_name: &str,
    delete_deps: &[String],
    ovr: &MergedCiOverride<'_>,
    dry_run: bool,
) -> Result<usize, String> {
    let (_, mut doc) = manifest::read_manifest(member_manifest)?;
    let mut changes = 0;

    // Delete specific deps from this member
    for dep_name in delete_deps {
        for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
            if manifest::delete_dep(&mut doc, section, dep_name) {
                changes += 1;
            }
        }
    }

    // Strip features
    if let Some(features) = ovr.strip_features().get(member_name) {
        for feature in features {
            // Strip from all deps' features arrays
            for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
                // We need to find which dep has this feature and strip it
                // For now, strip from all deps in the section
                if let Some(deps) = doc.get(section).and_then(|s| s.as_table_like()) {
                    let dep_names: Vec<String> = deps.iter().map(|(k, _)| k.to_string()).collect();
                    for dep_name in dep_names {
                        if manifest::strip_dep_feature(&mut doc, section, &dep_name, feature) {
                            changes += 1;
                        }
                    }
                }
            }
        }
    }

    // Blank keys
    if let Some(blanks) = ovr.blank_keys().get(member_name) {
        for (key, value) in blanks {
            for section in &["dependencies", "dev-dependencies", "build-dependencies"] {
                if manifest::set_dep_value_raw(&mut doc, section, key, value) {
                    changes += 1;
                }
            }
        }
    }

    if changes > 0 {
        let wrote = manifest::write_manifest(member_manifest, &doc, dry_run)?;
        if wrote {
            let label = if dry_run { "[dry-run] " } else { "" };
            println!(
                "{label}{}: {changes} member changes",
                member_manifest.display()
            );
        }
    }

    Ok(changes)
}

/// Convert DepSection to the TOML section key string
fn dep_section_key(section: DepSection) -> &'static str {
    match section {
        DepSection::Dependencies => "dependencies",
        DepSection::DevDependencies => "dev-dependencies",
        DepSection::BuildDependencies => "build-dependencies",
        DepSection::WorkspaceDependencies => "workspace.dependencies",
    }
}

/// Get the git URL for a dependency crate.
/// When the crate is in the ecosystem, uses its repo_dir for URL resolution.
/// When not (CI without full checkout), infers repo from the dep's path value.
pub(crate) fn dep_git_url(
    dep_name: &str,
    dep_path: Option<&str>,
    eco: &Ecosystem,
    config: &SuperworkConfig,
) -> Option<String> {
    // Look up the crate's repo and get its GitHub URL
    if let Some(info) = eco.crates.get(dep_name) {
        return config.github_url_for(&info.repo_dir);
    }

    // Fallback: infer repo dir from the path value.
    // "../zensally/crates/zensally-zentract" → repo dir is "../zensally"
    if let Some(path_str) = dep_path {
        let components: Vec<&str> = path_str.split('/').collect();
        if components.len() >= 2 && components[0] == ".." {
            let repo_dir = format!("../{}", components[1]);
            return config.github_url_for(&repo_dir);
        }
    }

    // Last resort: dep name = repo name
    Some(format!(
        "https://github.com/{}/{}",
        config.meta().default_github_org,
        dep_name
    ))
}

/// Find a workspace member's Cargo.toml given the parent crate info and member name
fn find_member_manifest(
    ecosystem_root: &Path,
    parent: &discover::CrateInfo,
    member_name: &str,
) -> Result<Option<std::path::PathBuf>, String> {
    // The member name might be the crate name or a relative path
    let parent_dir = parent.manifest_path.parent().unwrap();

    // Try: parent_dir/member_name/Cargo.toml
    let candidate = parent_dir.join(member_name).join("Cargo.toml");
    if candidate.exists() {
        return Ok(Some(candidate));
    }

    // Try: look up by crate name in the ecosystem
    let _ = ecosystem_root;
    // Just search for a manifest that has this package name under the same repo
    let repo_dir = &parent.repo_dir;
    let pattern = format!(
        "{}/{}",
        ecosystem_root.join(repo_dir).display(),
        "*/Cargo.toml"
    );
    // Simple: walk subdirectories
    let repo_path = ecosystem_root.join(repo_dir);
    if let Ok(entries) = std::fs::read_dir(&repo_path) {
        for entry in entries.flatten() {
            let toml = entry.path().join("Cargo.toml");
            if toml.exists() {
                if let Ok(content) = std::fs::read_to_string(&toml) {
                    if let Ok(doc) = content.parse::<toml::Value>() {
                        if let Some(name) = doc
                            .get("package")
                            .and_then(|p| p.as_table())
                            .and_then(|p| p.get("name"))
                            .and_then(|n| n.as_str())
                        {
                            if name == member_name {
                                return Ok(Some(toml));
                            }
                        }
                    }
                }
            }
        }
    }

    let _ = pattern;
    Ok(None)
}

/// Lexically normalize a path: resolve `.` and `..` without touching the filesystem.
/// This is needed because `canonicalize()` fails for paths that don't exist (common in CI).
fn normalize_path(path: &Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut result = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other),
        }
    }
    result
}
