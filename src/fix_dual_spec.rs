//! Add version specs to path-only internal dependencies.
//!
//! Scans all internal deps and adds `version = "{current_version}"` alongside
//! existing `path` keys, making them dual-specified and ready for publish.
//! Handles workspace-inherited deps by editing the workspace root's
//! `[workspace.dependencies]` table.

use crate::config::SuperworkConfig;
use crate::discover::{self, DepSection};
use crate::manifest;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

pub fn run(
    ecosystem_root: &Path,
    config: &SuperworkConfig,
    filter: Option<&str>,
    target_crate: Option<&str>,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;

    // Collect path-only deps between publishable crates
    let mut to_fix: Vec<&discover::InternalDep> = eco
        .deps
        .iter()
        .filter(|d| {
            d.has_path
                && !d.has_version
                && eco.crates.get(&d.from_crate).is_some_and(|c| c.publishable)
                && eco.crates.get(&d.to_crate).is_some_and(|c| c.publishable)
        })
        .collect();

    // Apply filters
    if let Some(f) = filter {
        to_fix.retain(|d| glob_match(f, &d.from_crate));
    }
    if let Some(t) = target_crate {
        to_fix.retain(|d| d.to_crate == t);
    }

    if to_fix.is_empty() {
        println!("No path-only deps to fix.");
        return Ok(());
    }

    println!(
        "Found {} path-only deps between publishable crates",
        to_fix.len()
    );

    // First pass: detect which deps are workspace-inherited so we edit the right file.
    // Group edits by the file that needs to be modified.
    // Key: (manifest_path_to_edit, Option<workspace_dep_name_or_section>)
    struct Edit {
        /// The manifest to edit
        manifest_path: PathBuf,
        /// If Some, edit workspace.dependencies[dep_name] in this file.
        /// If None, edit the dep in the regular section.
        workspace_dep: bool,
        section: DepSection,
        dep_name: String,
        version: String,
    }

    let mut edits: Vec<Edit> = Vec::new();
    // Track workspace deps we've already scheduled to avoid duplicates
    // (multiple members can reference the same workspace.dependencies entry)
    let mut ws_dep_seen: BTreeSet<(PathBuf, String)> = BTreeSet::new();

    for dep in &to_fix {
        let target_version = &eco.crates[&dep.to_crate].version;
        let from_info = &eco.crates[&dep.from_crate];

        // Check if this dep uses workspace = true in the actual manifest
        let is_workspace_inherited = is_dep_workspace_inherited(
            &dep.manifest_path,
            dep_section_key(dep.section),
            &dep.dep_key,
        )?;

        if is_workspace_inherited {
            // Edit the workspace root's [workspace.dependencies] instead
            let ws_root = from_info.workspace_root.as_ref().ok_or_else(|| {
                format!(
                    "{} -> {}: workspace-inherited dep but no workspace root found",
                    dep.from_crate, dep.to_crate
                )
            })?;

            let key = (ws_root.clone(), dep.dep_key.clone());
            if ws_dep_seen.contains(&key) {
                continue; // Already scheduled
            }
            ws_dep_seen.insert(key);

            edits.push(Edit {
                manifest_path: ws_root.clone(),
                workspace_dep: true,
                section: dep.section,
                dep_name: dep.dep_key.clone(),
                version: target_version.clone(),
            });
        } else {
            edits.push(Edit {
                manifest_path: dep.manifest_path.clone(),
                workspace_dep: false,
                section: dep.section,
                dep_name: dep.dep_key.clone(),
                version: target_version.clone(),
            });
        }
    }

    // Group edits by manifest path for batched I/O
    let mut by_manifest: BTreeMap<PathBuf, Vec<&Edit>> = BTreeMap::new();
    for edit in &edits {
        by_manifest
            .entry(edit.manifest_path.clone())
            .or_default()
            .push(edit);
    }

    let label = if dry_run { "[dry-run] " } else { "" };
    let mut files_modified = 0;
    let mut deps_fixed = 0;

    for (manifest_path, file_edits) in &by_manifest {
        let (_, mut doc) = manifest::read_manifest(manifest_path)?;
        let mut file_changes = 0;

        for edit in file_edits {
            let changed = if edit.workspace_dep {
                set_workspace_dep_version(&mut doc, &edit.dep_name, &edit.version)
            } else {
                manifest::set_dep_version(
                    &mut doc,
                    dep_section_key(edit.section),
                    &edit.dep_name,
                    &edit.version,
                )
            };
            if changed {
                file_changes += 1;
                deps_fixed += 1;
            }
        }

        if file_changes > 0 {
            let wrote = manifest::write_manifest(manifest_path, &doc, dry_run)?;
            if wrote {
                let dep_names: Vec<&str> = file_edits.iter().map(|e| e.dep_name.as_str()).collect();
                println!(
                    "{label}  {} ({} deps: {})",
                    manifest_path.display(),
                    file_changes,
                    dep_names.join(", ")
                );
                files_modified += 1;
            }
        }
    }

    if dry_run {
        println!("{label}Would fix {deps_fixed} deps across {files_modified} files");
    } else {
        println!("Fixed {deps_fixed} deps across {files_modified} files");
    }

    Ok(())
}

/// Check if a dependency entry in a manifest uses `workspace = true`
fn is_dep_workspace_inherited(
    manifest_path: &Path,
    section: &str,
    dep_name: &str,
) -> Result<bool, String> {
    let content = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("reading {}: {e}", manifest_path.display()))?;
    let doc: toml::Value =
        toml::from_str(&content).map_err(|e| format!("parsing {}: {e}", manifest_path.display()))?;

    let result = doc
        .get(section)
        .and_then(|s| s.as_table())
        .and_then(|t| t.get(dep_name))
        .and_then(|d| d.as_table())
        .and_then(|t| t.get("workspace"))
        .and_then(|w| w.as_bool())
        .unwrap_or(false);

    Ok(result)
}

/// Add version to a workspace.dependencies entry (navigates doc["workspace"]["dependencies"])
fn set_workspace_dep_version(
    doc: &mut toml_edit::DocumentMut,
    dep_name: &str,
    version: &str,
) -> bool {
    let Some(ws) = doc.get_mut("workspace").and_then(|w| w.as_table_mut()) else {
        return false;
    };
    let Some(ws_deps) = ws
        .get_mut("dependencies")
        .and_then(|d| d.as_table_like_mut())
    else {
        return false;
    };
    let Some(dep_entry) = ws_deps.get_mut(dep_name) else {
        return false;
    };

    if let Some(tbl) = dep_entry.as_inline_table_mut() {
        let existing = tbl
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        if existing.as_deref() == Some(version) {
            return false;
        }
        tbl.insert("version", version.into());
        true
    } else if let Some(tbl) = dep_entry.as_table_mut() {
        let existing = tbl
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        if existing.as_deref() == Some(version) {
            return false;
        }
        tbl.insert("version", toml_edit::value(version));
        true
    } else if dep_entry.as_str().is_some() {
        // Bare string — this IS the version, shouldn't need path. Skip.
        false
    } else {
        false
    }
}

fn dep_section_key(section: DepSection) -> &'static str {
    match section {
        DepSection::Dependencies => "dependencies",
        DepSection::DevDependencies => "dev-dependencies",
        DepSection::BuildDependencies => "build-dependencies",
        DepSection::WorkspaceDependencies => "dependencies", // shouldn't reach here
    }
}

fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    if pattern.contains('*') {
        let parts: Vec<&str> = pattern.split('*').collect();
        if parts.len() == 2 {
            return name.starts_with(parts[0]) && name.ends_with(parts[1]);
        }
    }
    pattern == name
}
