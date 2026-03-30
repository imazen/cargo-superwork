use crate::config::EcosystemConfig;
use crate::discover::{self, DepSection};
use crate::manifest;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub fn run(
    ecosystem_root: &Path,
    config: &EcosystemConfig,
    crate_name: &str,
    new_version: &str,
    dry_run: bool,
) -> Result<(), String> {
    // Validate version
    semver::Version::parse(new_version)
        .map_err(|e| format!("invalid version '{new_version}': {e}"))?;

    let eco = discover::scan_ecosystem(ecosystem_root, config)?;

    let target = eco
        .crates
        .get(crate_name)
        .ok_or_else(|| format!("crate '{crate_name}' not found in ecosystem"))?;

    println!(
        "Bumping {} from {} to {new_version}",
        crate_name, target.version
    );

    let mut files_modified = 0;

    // 1. Update the crate's own version
    {
        let (_, mut doc) = manifest::read_manifest(&target.manifest_path)?;
        if manifest::set_package_version(&mut doc, new_version) {
            let wrote = manifest::write_manifest(&target.manifest_path, &doc, dry_run)?;
            if wrote {
                let label = if dry_run { "[dry-run] " } else { "" };
                println!(
                    "{label}  {} [package].version",
                    target.manifest_path.display()
                );
                files_modified += 1;
            }
        }
    }

    // 2. If in a workspace with workspace.package.version, update that too
    if let Some(ws_root) = &target.workspace_root {
        let (_, mut doc) = manifest::read_manifest(ws_root)?;
        if let Some(ws) = doc.get_mut("workspace").and_then(|w| w.as_table_mut()) {
            if let Some(pkg) = ws.get_mut("package").and_then(|p| p.as_table_mut()) {
                if pkg.get("version").is_some() {
                    pkg.insert("version", toml_edit::value(new_version));
                    let wrote = manifest::write_manifest(ws_root, &doc, dry_run)?;
                    if wrote {
                        let label = if dry_run { "[dry-run] " } else { "" };
                        println!("{label}  {} [workspace.package].version", ws_root.display());
                        files_modified += 1;
                    }
                }
            }
        }
    }

    // 3. Update all dependents' version requirements
    // Group deps by manifest path to avoid reading the same file multiple times
    let mut by_manifest: BTreeMap<PathBuf, Vec<&discover::InternalDep>> = BTreeMap::new();
    for dep in &eco.deps {
        if dep.to_crate == crate_name && dep.has_version {
            by_manifest
                .entry(dep.manifest_path.clone())
                .or_default()
                .push(dep);
        }
    }

    for (manifest_path, deps) in &by_manifest {
        let (_, mut doc) = manifest::read_manifest(manifest_path)?;
        let mut file_changes = 0;

        for dep in deps {
            let section = match dep.section {
                DepSection::Dependencies => "dependencies",
                DepSection::DevDependencies => "dev-dependencies",
                DepSection::BuildDependencies => "build-dependencies",
                DepSection::WorkspaceDependencies => {
                    // Handle workspace.dependencies specially
                    if let Some(ws) = doc.get_mut("workspace").and_then(|w| w.as_table_mut()) {
                        if let Some(ws_deps) = ws
                            .get_mut("dependencies")
                            .and_then(|d| d.as_table_like_mut())
                        {
                            if let Some(dep_entry) = ws_deps.get_mut(crate_name) {
                                if let Some(tbl) = dep_entry.as_inline_table_mut() {
                                    tbl.insert("version", new_version.into());
                                    file_changes += 1;
                                } else if let Some(tbl) = dep_entry.as_table_mut() {
                                    tbl.insert("version", toml_edit::value(new_version));
                                    file_changes += 1;
                                } else if dep_entry.as_str().is_some() {
                                    *dep_entry = toml_edit::Item::Value(new_version.into());
                                    file_changes += 1;
                                }
                            }
                        }
                    }
                    continue;
                }
            };

            if manifest::set_dep_version(&mut doc, section, crate_name, new_version) {
                file_changes += 1;
            }
        }

        if file_changes > 0 {
            let wrote = manifest::write_manifest(manifest_path, &doc, dry_run)?;
            if wrote {
                let label = if dry_run { "[dry-run] " } else { "" };
                let dep_names: Vec<&str> = deps.iter().map(|d| d.from_crate.as_str()).collect();
                println!(
                    "{label}  {} ({})",
                    manifest_path.display(),
                    dep_names.join(", ")
                );
                files_modified += 1;
            }
        }
    }

    if dry_run {
        println!("[dry-run] would modify {files_modified} files");
    } else {
        println!("modified {files_modified} files");
    }

    Ok(())
}
