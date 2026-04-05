use crate::config::{CiCrateOverride, CrateClass, SuperworkConfig};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A crate discovered in the ecosystem
#[derive(Debug, Clone)]
pub struct CrateInfo {
    pub name: String,
    pub version: String,
    pub manifest_path: PathBuf,
    /// Repo directory relative to ecosystem root (e.g., "zencodecs" or "../archmage")
    pub repo_dir: String,
    /// Whether this crate can be published to crates.io
    pub publishable: bool,
    /// GitHub URL for this repo
    pub github_url: Option<String>,
    /// If this crate is a workspace member, the workspace root Cargo.toml path
    pub workspace_root: Option<PathBuf>,
    /// CI overrides from [package.metadata.superwork.ci] in this crate's Cargo.toml
    pub inline_ci: Option<CiCrateOverride>,
    /// Check overrides from [package.metadata.superwork.checks]
    pub inline_checks: Option<BTreeMap<String, String>>,
    /// Release classification (library, binary, abi)
    pub class: CrateClass,
}

/// Which section a dependency appears in
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepSection {
    Dependencies,
    DevDependencies,
    BuildDependencies,
    WorkspaceDependencies,
}

impl std::fmt::Display for DepSection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Dependencies => write!(f, "dependencies"),
            Self::DevDependencies => write!(f, "dev-dependencies"),
            Self::BuildDependencies => write!(f, "build-dependencies"),
            Self::WorkspaceDependencies => write!(f, "workspace.dependencies"),
        }
    }
}

/// A dependency relationship between ecosystem crates
#[derive(Debug, Clone)]
pub struct InternalDep {
    /// The crate that has the dependency
    pub from_crate: String,
    /// The dependency crate name (resolved from `package` rename if present)
    pub to_crate: String,
    /// The key name as written in the TOML (may differ from to_crate when `package` is used)
    pub dep_key: String,
    pub section: DepSection,
    pub has_version: bool,
    pub has_path: bool,
    pub is_optional: bool,
    /// The path value as written in Cargo.toml
    pub path_value: Option<String>,
    /// The version value as written in Cargo.toml
    pub version_value: Option<String>,
    /// The manifest file this dep is declared in
    pub manifest_path: PathBuf,
}

/// Complete ecosystem state
#[derive(Debug)]
pub struct Ecosystem {
    pub root: PathBuf,
    pub crates: BTreeMap<String, CrateInfo>,
    pub deps: Vec<InternalDep>,
}

/// Scan the ecosystem and build the crate registry
pub fn scan_ecosystem(
    ecosystem_root: &Path,
    config: &SuperworkConfig,
) -> Result<Ecosystem, String> {
    let mut crates = BTreeMap::new();

    // Scan main root and extra roots
    let scan_roots = config.scan_roots(ecosystem_root);

    for scan_root in &scan_roots {
        let is_main_root = scan_root == ecosystem_root;
        scan_root_dir(scan_root, ecosystem_root, is_main_root, config, &mut crates)?;
    }

    // Now scan for internal dependencies
    let crate_names: std::collections::BTreeSet<String> = crates.keys().cloned().collect();
    let mut deps = Vec::new();

    // Pre-parse workspace root dependency tables for resolving { workspace = true }
    let mut ws_dep_tables: BTreeMap<PathBuf, toml::value::Table> = BTreeMap::new();
    for info in crates.values() {
        if let Some(ws_root) = &info.workspace_root {
            if !ws_dep_tables.contains_key(ws_root) {
                if let Ok(content) = std::fs::read_to_string(ws_root) {
                    if let Ok(doc) = content.parse::<toml::Value>() {
                        if let Some(ws_deps) = doc
                            .get("workspace")
                            .and_then(|w| w.as_table())
                            .and_then(|w| w.get("dependencies"))
                            .and_then(|d| d.as_table())
                        {
                            ws_dep_tables.insert(ws_root.clone(), ws_deps.clone());
                        }
                    }
                }
            }
        }
    }

    for info in crates.values() {
        let ws_deps = info
            .workspace_root
            .as_ref()
            .and_then(|ws| ws_dep_tables.get(ws));
        scan_deps_in_manifest(
            &info.manifest_path,
            &info.name,
            &crate_names,
            ws_deps,
            &mut deps,
        )?;
    }

    Ok(Ecosystem {
        root: ecosystem_root.to_path_buf(),
        crates,
        deps,
    })
}

/// Scan a root directory for Cargo.toml files
fn scan_root_dir(
    scan_root: &Path,
    ecosystem_root: &Path,
    is_main_root: bool,
    config: &SuperworkConfig,
    crates: &mut BTreeMap<String, CrateInfo>,
) -> Result<(), String> {
    if !scan_root.is_dir() {
        return Ok(());
    }

    // For the main root, scan each subdirectory.
    // For extra roots, scan the root itself (it IS a repo).
    if is_main_root {
        let entries = std::fs::read_dir(scan_root)
            .map_err(|e| format!("reading {}: {e}", scan_root.display()))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("reading dir entry: {e}"))?;
            let path = entry.path();

            // Skip non-directories, target/, .cargo/, .claude/, retired/
            if !path.is_dir() {
                continue;
            }
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(name, "target" | ".cargo" | ".claude" | "retired") {
                continue;
            }

            // Follow symlinks
            let path = match path.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };

            scan_repo_dir(&path, ecosystem_root, config, crates)?;
        }
    } else {
        // Extra root: scan it as a single repo
        let path = match scan_root.canonicalize() {
            Ok(p) => p,
            Err(_) => return Ok(()),
        };
        scan_repo_dir(&path, ecosystem_root, config, crates)?;
    }

    Ok(())
}

/// Scan a single repo directory for crates
fn scan_repo_dir(
    repo_path: &Path,
    ecosystem_root: &Path,
    config: &SuperworkConfig,
    crates: &mut BTreeMap<String, CrateInfo>,
) -> Result<(), String> {
    let cargo_toml = repo_path.join("Cargo.toml");
    if !cargo_toml.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&cargo_toml)
        .map_err(|e| format!("reading {}: {e}", cargo_toml.display()))?;

    let doc: toml::Value =
        toml::from_str(&content).map_err(|e| format!("parsing {}: {e}", cargo_toml.display()))?;

    let repo_dir = relative_dir(repo_path, ecosystem_root);

    // Check if this is a workspace
    if let Some(workspace) = doc.get("workspace").and_then(|w| w.as_table()) {
        // Scan workspace members
        if let Some(members) = workspace.get("members").and_then(|m| m.as_array()) {
            for member in members {
                if let Some(member_str) = member.as_str() {
                    // Expand globs
                    let pattern = repo_path.join(member_str);
                    let pattern_str = pattern.to_string_lossy();

                    let paths = glob_paths(&pattern_str);
                    for member_path in paths {
                        let member_toml = member_path.join("Cargo.toml");
                        if member_toml.exists() {
                            scan_single_crate(
                                &member_toml,
                                &repo_dir,
                                ecosystem_root,
                                config,
                                Some(&cargo_toml),
                                crates,
                            )?;
                        }
                    }
                }
            }
        }
    }

    // Also check if the root Cargo.toml itself is a package
    if doc.get("package").is_some() {
        scan_single_crate(&cargo_toml, &repo_dir, ecosystem_root, config, None, crates)?;
    }

    Ok(())
}

/// Parse a single crate's Cargo.toml and add to registry
fn scan_single_crate(
    manifest_path: &Path,
    repo_dir: &str,
    ecosystem_root: &Path,
    config: &SuperworkConfig,
    workspace_root: Option<&Path>,
    crates: &mut BTreeMap<String, CrateInfo>,
) -> Result<(), String> {
    let content = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("reading {}: {e}", manifest_path.display()))?;

    let doc: toml::Value = toml::from_str(&content)
        .map_err(|e| format!("parsing {}: {e}", manifest_path.display()))?;

    let package = match doc.get("package").and_then(|p| p.as_table()) {
        Some(p) => p,
        None => return Ok(()), // Not a package (workspace root only)
    };

    let name = package
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| format!("no package.name in {}", manifest_path.display()))?
        .to_string();

    let version = package
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();

    // Check publish field
    let has_publish_false = package
        .get("publish")
        .map(|p| {
            if let Some(b) = p.as_bool() {
                !b
            } else if let Some(arr) = p.as_array() {
                arr.is_empty()
            } else {
                false
            }
        })
        .unwrap_or(false);

    let publishable = !has_publish_false && !config.unpublished.crates.contains(&name);

    let github_url = config.github_url_for(repo_dir);

    let _ = ecosystem_root; // used in repo_dir computation already

    // Extract [package.metadata.superwork] if present
    let superwork_meta = package
        .get("metadata")
        .and_then(|m| m.as_table())
        .and_then(|m| m.get("superwork"))
        .and_then(|s| s.as_table());

    // Also check [workspace.metadata.superwork] for workspace roots
    let ws_superwork_meta = if workspace_root.is_none() {
        // This IS a workspace root (or standalone) — check workspace.metadata
        doc.get("workspace")
            .and_then(|w| w.as_table())
            .and_then(|w| w.get("metadata"))
            .and_then(|m| m.as_table())
            .and_then(|m| m.get("superwork"))
            .and_then(|s| s.as_table())
    } else {
        None
    };

    let meta = superwork_meta.or(ws_superwork_meta);

    // Parse CI overrides from inline metadata
    let inline_ci = meta.and_then(|m| m.get("ci")).and_then(|ci| {
        // Serialize back to string, then deserialize as CiCrateOverride
        let ci_str = toml::to_string(ci).ok()?;
        toml::from_str::<CiCrateOverride>(&ci_str).ok()
    });

    // Parse check overrides
    let inline_checks = meta
        .and_then(|m| m.get("checks"))
        .and_then(|c| c.as_table())
        .map(|t| {
            t.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        });

    // Determine crate class: config override > heuristic
    let class = if let Some(c) = config.release.crate_class.get(&name) {
        *c
    } else if name.ends_with("_abi") || name.ends_with("-ffi") || name.ends_with("_ffi") {
        CrateClass::Abi
    } else {
        // Check if binary-only: has [[bin]] but no [lib]
        let has_bin = doc.get("bin").and_then(|b| b.as_array()).is_some_and(|a| !a.is_empty());
        let has_lib = doc.get("lib").is_some()
            || manifest_path.parent().is_some_and(|p| p.join("src/lib.rs").exists());
        if has_bin && !has_lib {
            CrateClass::Binary
        } else {
            CrateClass::Library
        }
    };

    crates.insert(
        name.clone(),
        CrateInfo {
            name,
            version,
            manifest_path: manifest_path.to_path_buf(),
            repo_dir: repo_dir.to_string(),
            publishable,
            github_url,
            workspace_root: workspace_root.map(PathBuf::from),
            inline_ci,
            inline_checks,
            class,
        },
    );

    Ok(())
}

/// Scan a single Cargo.toml for dependencies on internal crates
fn scan_deps_in_manifest(
    manifest_path: &Path,
    crate_name: &str,
    known_crates: &std::collections::BTreeSet<String>,
    ws_deps: Option<&toml::value::Table>,
    deps: &mut Vec<InternalDep>,
) -> Result<(), String> {
    let content = std::fs::read_to_string(manifest_path)
        .map_err(|e| format!("reading {}: {e}", manifest_path.display()))?;

    let doc: toml::Value = toml::from_str(&content)
        .map_err(|e| format!("parsing {}: {e}", manifest_path.display()))?;

    let sections = [
        ("dependencies", DepSection::Dependencies),
        ("dev-dependencies", DepSection::DevDependencies),
        ("build-dependencies", DepSection::BuildDependencies),
    ];

    for (key, section) in &sections {
        if let Some(table) = doc.get(key).and_then(|d| d.as_table()) {
            for (dep_name, dep_value) in table {
                // Check if dep_name matches a known crate, or uses `package` rename
                let actual_name = dep_value
                    .as_table()
                    .and_then(|t| t.get("package"))
                    .and_then(|p| p.as_str())
                    .unwrap_or(dep_name);

                if !known_crates.contains(actual_name) {
                    // Also check if it has a path that looks internal (../)
                    let has_internal_path = dep_value
                        .as_table()
                        .and_then(|t| t.get("path"))
                        .and_then(|p| p.as_str())
                        .is_some_and(|p| p.starts_with(".."));
                    if !has_internal_path {
                        continue;
                    }
                }

                let (has_version, version_value) = extract_version(dep_value);
                let (has_path, path_value) = extract_path(dep_value);
                let is_optional = dep_value
                    .as_table()
                    .and_then(|t| t.get("optional"))
                    .and_then(|o| o.as_bool())
                    .unwrap_or(false);

                // Check for workspace = true — resolve from workspace.dependencies
                let is_workspace_ref = dep_value
                    .as_table()
                    .and_then(|t| t.get("workspace"))
                    .and_then(|w| w.as_bool())
                    .unwrap_or(false);

                if is_workspace_ref {
                    // Resolve the actual dep spec from workspace.dependencies
                    if let Some(ws_table) = ws_deps {
                        if let Some(ws_dep_value) = ws_table.get(dep_name) {
                            let ws_actual_name = ws_dep_value
                                .as_table()
                                .and_then(|t| t.get("package"))
                                .and_then(|p| p.as_str())
                                .unwrap_or(dep_name);

                            if !known_crates.contains(ws_actual_name) {
                                let has_internal_path = ws_dep_value
                                    .as_table()
                                    .and_then(|t| t.get("path"))
                                    .and_then(|p| p.as_str())
                                    .is_some_and(|p| p.starts_with(".."));
                                if !has_internal_path {
                                    continue;
                                }
                            }

                            let (ws_has_version, ws_version_value) = extract_version(ws_dep_value);
                            let (ws_has_path, ws_path_value) = extract_path(ws_dep_value);
                            let ws_is_optional = dep_value
                                .as_table()
                                .and_then(|t| t.get("optional"))
                                .and_then(|o| o.as_bool())
                                .unwrap_or(false);

                            deps.push(InternalDep {
                                from_crate: crate_name.to_string(),
                                to_crate: ws_actual_name.to_string(),
                                dep_key: dep_name.clone(),
                                section: *section,
                                has_version: ws_has_version,
                                has_path: ws_has_path,
                                is_optional: ws_is_optional,
                                path_value: ws_path_value,
                                version_value: ws_version_value,
                                manifest_path: manifest_path.to_path_buf(),
                            });
                        }
                    }
                    continue;
                }

                deps.push(InternalDep {
                    from_crate: crate_name.to_string(),
                    to_crate: actual_name.to_string(),
                    dep_key: dep_name.clone(),
                    section: *section,
                    has_version,
                    has_path,
                    is_optional,
                    path_value,
                    version_value,
                    manifest_path: manifest_path.to_path_buf(),
                });
            }
        }
    }

    Ok(())
}

fn extract_version(v: &toml::Value) -> (bool, Option<String>) {
    if let Some(s) = v.as_str() {
        return (true, Some(s.to_string()));
    }
    if let Some(t) = v.as_table() {
        if let Some(ver) = t.get("version").and_then(|v| v.as_str()) {
            return (true, Some(ver.to_string()));
        }
    }
    (false, None)
}

fn extract_path(v: &toml::Value) -> (bool, Option<String>) {
    if let Some(t) = v.as_table() {
        if let Some(p) = t.get("path").and_then(|p| p.as_str()) {
            return (true, Some(p.to_string()));
        }
    }
    (false, None)
}

/// Compute relative directory from ecosystem root
fn relative_dir(path: &Path, root: &Path) -> String {
    // Try to make it relative to root
    if let Ok(rel) = path.strip_prefix(root) {
        rel.to_string_lossy().to_string()
    } else if let (Ok(abs_path), Ok(abs_root)) = (path.canonicalize(), root.canonicalize()) {
        // Try with canonical paths
        if let Ok(rel) = abs_path.strip_prefix(&abs_root) {
            rel.to_string_lossy().to_string()
        } else {
            // Use relative path with ../
            pathdiff_relative(&abs_root, &abs_path)
        }
    } else {
        path.to_string_lossy().to_string()
    }
}

fn pathdiff_relative(from: &Path, to: &Path) -> String {
    // Simple relative path computation
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

/// Expand glob patterns in paths (handles * wildcards in workspace member declarations)
fn glob_paths(pattern: &str) -> Vec<PathBuf> {
    // If no glob characters, just return the path directly
    if !pattern.contains('*') && !pattern.contains('?') {
        let p = PathBuf::from(pattern);
        if p.exists() {
            return vec![p];
        }
        return vec![];
    }

    // Simple glob expansion for workspace member patterns like "crates/*"
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() != 2 {
        // Complex glob, skip
        return vec![];
    }

    let prefix = PathBuf::from(parts[0]);
    let suffix = parts[1];

    let parent =
        if prefix.to_string_lossy().ends_with('/') || prefix.to_string_lossy().ends_with('\\') {
            prefix.clone()
        } else {
            match prefix.parent() {
                Some(p) => p.to_path_buf(),
                None => return vec![],
            }
        };

    if !parent.is_dir() {
        return vec![];
    }

    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&parent) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let full = if suffix.is_empty() {
                    path.clone()
                } else {
                    path.join(suffix.trim_start_matches('/'))
                };
                if full.exists() || suffix.is_empty() {
                    results.push(path);
                }
            }
        }
    }
    results
}

/// Run the discover command
pub fn run(ecosystem_root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = scan_ecosystem(ecosystem_root, config)?;

    println!("Ecosystem root: {}", ecosystem_root.display());
    println!();

    // Group crates by repo
    let mut by_repo: BTreeMap<&str, Vec<&CrateInfo>> = BTreeMap::new();
    for info in eco.crates.values() {
        by_repo.entry(&info.repo_dir).or_default().push(info);
    }

    println!("=== Repos ({}) ===", by_repo.len());
    for (repo, repo_crates) in &by_repo {
        let names: Vec<&str> = repo_crates.iter().map(|c| c.name.as_str()).collect();
        let pub_count = repo_crates.iter().filter(|c| c.publishable).count();
        let status = if pub_count == repo_crates.len() {
            "".to_string()
        } else {
            format!(" ({pub_count}/{} publishable)", repo_crates.len())
        };
        println!("  {repo}: {}{status}", names.join(", "));
    }

    println!();
    println!("=== Crates ({}) ===", eco.crates.len());
    let publishable = eco.crates.values().filter(|c| c.publishable).count();
    let unpublishable = eco.crates.len() - publishable;
    println!("  {publishable} publishable, {unpublishable} unpublishable");

    println!();
    println!("=== Internal Dependencies ({}) ===", eco.deps.len());
    let dual = eco
        .deps
        .iter()
        .filter(|d| d.has_version && d.has_path)
        .count();
    let path_only = eco
        .deps
        .iter()
        .filter(|d| d.has_path && !d.has_version)
        .count();
    let version_only = eco
        .deps
        .iter()
        .filter(|d| d.has_version && !d.has_path)
        .count();
    println!("  {dual} dual-specified (version + path)");
    println!("  {path_only} path-only (will block publish)");
    println!("  {version_only} version-only (no local override)");

    // Show most-depended-on crates
    let mut dep_counts: BTreeMap<&str, usize> = BTreeMap::new();
    for dep in &eco.deps {
        *dep_counts.entry(&dep.to_crate).or_default() += 1;
    }
    let mut sorted: Vec<_> = dep_counts.into_iter().collect();
    sorted.sort_by(|a, b| b.1.cmp(&a.1));

    println!();
    println!("=== Most Depended-On ===");
    for (name, count) in sorted.iter().take(15) {
        println!("  {name}: {count} dependents");
    }

    Ok(())
}
