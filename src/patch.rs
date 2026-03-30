use crate::config::SuperworkConfig;
use crate::discover::{self, DepSection};
use crate::manifest;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Add path overrides to all internal deps (dev mode)
pub fn run_patch(
    ecosystem_root: &Path,
    config: &SuperworkConfig,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;
    let mut files_modified = 0;
    let mut changes = 0;

    // Group deps by manifest file
    let mut by_manifest: BTreeMap<PathBuf, Vec<&discover::InternalDep>> = BTreeMap::new();
    for dep in &eco.deps {
        by_manifest
            .entry(dep.manifest_path.clone())
            .or_default()
            .push(dep);
    }

    for (manifest_path, deps) in &by_manifest {
        let (_, mut doc) = manifest::read_manifest(manifest_path)?;
        let mut file_changes = 0;

        for dep in deps {
            // Skip if dep target not in ecosystem
            let target = match eco.crates.get(&dep.to_crate) {
                Some(c) => c,
                None => continue,
            };

            // Compute relative path from this manifest to the target's Cargo.toml parent
            let from_dir = manifest_path.parent().unwrap();
            let to_dir = target.manifest_path.parent().unwrap();
            let rel_path = compute_relative_path(from_dir, to_dir);

            let section = dep_section_key(dep.section);
            if manifest::set_dep_path(&mut doc, section, &dep.to_crate, &rel_path) {
                file_changes += 1;
            }
        }

        if file_changes > 0 {
            let wrote = manifest::write_manifest(manifest_path, &doc, dry_run)?;
            if wrote {
                let label = if dry_run { "[dry-run] " } else { "" };
                println!(
                    "{label}{}: {file_changes} paths added/updated",
                    manifest_path.display()
                );
                files_modified += 1;
                changes += file_changes;
            }
        }
    }

    if dry_run {
        println!("[dry-run] would modify {files_modified} files ({changes} changes)");
    } else {
        println!("modified {files_modified} files ({changes} changes)");
    }

    Ok(())
}

/// Remove path overrides from dual-specified deps (publish mode)
pub fn run_unpatch(
    ecosystem_root: &Path,
    config: &SuperworkConfig,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;
    let mut files_modified = 0;
    let mut changes = 0;
    let mut blocked = 0;

    // Group deps by manifest file
    let mut by_manifest: BTreeMap<PathBuf, Vec<&discover::InternalDep>> = BTreeMap::new();
    for dep in &eco.deps {
        by_manifest
            .entry(dep.manifest_path.clone())
            .or_default()
            .push(dep);
    }

    for (manifest_path, deps) in &by_manifest {
        let (_, mut doc) = manifest::read_manifest(manifest_path)?;
        let mut file_changes = 0;

        for dep in deps {
            if !dep.has_path {
                continue;
            }

            if dep.has_version {
                // Dual-specified: safe to remove path
                let section = dep_section_key(dep.section);
                if manifest::remove_dep_path(&mut doc, section, &dep.to_crate) {
                    file_changes += 1;
                }
            } else {
                // Path-only: can't unpatch without a version
                let to_publishable = eco.crates.get(&dep.to_crate).is_some_and(|c| c.publishable);
                if to_publishable {
                    eprintln!(
                        "  blocked: {} -> {} is path-only (needs version before unpatch)",
                        dep.from_crate, dep.to_crate
                    );
                    blocked += 1;
                }
            }
        }

        if file_changes > 0 {
            let wrote = manifest::write_manifest(manifest_path, &doc, dry_run)?;
            if wrote {
                let label = if dry_run { "[dry-run] " } else { "" };
                println!(
                    "{label}{}: {file_changes} paths removed",
                    manifest_path.display()
                );
                files_modified += 1;
                changes += file_changes;
            }
        }
    }

    if dry_run {
        println!("[dry-run] would modify {files_modified} files ({changes} changes)");
    } else {
        println!("modified {files_modified} files ({changes} changes)");
    }

    if blocked > 0 {
        eprintln!(
            "{blocked} deps blocked: add version specs first (run `cargo zen check` for details)"
        );
    }

    Ok(())
}

fn dep_section_key(section: DepSection) -> &'static str {
    match section {
        DepSection::Dependencies => "dependencies",
        DepSection::DevDependencies => "dev-dependencies",
        DepSection::BuildDependencies => "build-dependencies",
        DepSection::WorkspaceDependencies => "workspace.dependencies",
    }
}

/// Compute relative path from one directory to another
fn compute_relative_path(from: &Path, to: &Path) -> String {
    // Canonicalize both paths
    let from = from.canonicalize().unwrap_or_else(|_| from.to_path_buf());
    let to = to.canonicalize().unwrap_or_else(|_| to.to_path_buf());

    let from_components: Vec<_> = from.components().collect();
    let to_components: Vec<_> = to.components().collect();

    let common = from_components
        .iter()
        .zip(to_components.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let ups = from_components.len() - common;
    let mut result = PathBuf::new();
    for _ in 0..ups {
        result.push("..");
    }
    for component in &to_components[common..] {
        result.push(component);
    }

    result.to_string_lossy().to_string()
}
