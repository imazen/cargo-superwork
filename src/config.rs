use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Top-level ecosystem configuration from zen-ecosystem.toml
#[derive(Debug, Deserialize)]
pub struct EcosystemConfig {
    pub ecosystem: EcosystemMeta,
    #[serde(default)]
    pub repo: Vec<RepoOverride>,
    #[serde(default)]
    pub unpublished: UnpublishedConfig,
    #[serde(default)]
    pub ci: CiConfig,
}

#[derive(Debug, Deserialize)]
pub struct EcosystemMeta {
    /// Root directory (relative to config file location)
    #[serde(default = "default_root")]
    pub root: String,
    /// Default GitHub org for URL generation
    #[serde(default = "default_org")]
    pub default_github_org: String,
    /// Additional directories to scan (relative to root)
    #[serde(default)]
    pub extra_roots: Vec<String>,
}

fn default_root() -> String {
    ".".to_string()
}

fn default_org() -> String {
    "imazen".to_string()
}

/// Override for a repo whose GitHub URL differs from convention
#[derive(Debug, Deserialize)]
pub struct RepoOverride {
    /// Directory relative to ecosystem root
    pub dir: String,
    /// GitHub "org/repo" (e.g., "imazen/cavif-rs")
    #[serde(default)]
    pub github: Option<String>,
    /// If true, this repo has no remote
    #[serde(default)]
    pub no_remote: bool,
}

#[derive(Debug, Default, Deserialize)]
pub struct UnpublishedConfig {
    /// Crate names that should never be published
    #[serde(default)]
    pub crates: BTreeSet<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CiConfig {
    /// Default CI strategy for internal deps
    #[serde(default = "default_ci_strategy")]
    pub default_strategy: CiStrategy,
    /// Per-crate CI overrides
    #[serde(default)]
    pub overrides: BTreeMap<String, CiCrateOverride>,
}

fn default_ci_strategy() -> CiStrategy {
    CiStrategy::GitUrl
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiStrategy {
    /// Replace path= with git= URL
    #[default]
    GitUrl,
    /// Remove path= key, keep version= (requires dual-spec)
    StripPath,
    /// Delete the entire dependency line
    Delete,
}

/// Per-crate CI override configuration
#[derive(Debug, Default, Deserialize)]
pub struct CiCrateOverride {
    /// Default strategy for this crate's internal deps
    pub default_strategy: Option<CiStrategy>,
    /// Deps to delete entirely (unavailable in CI)
    #[serde(default)]
    pub delete: Vec<String>,
    /// TOML sections to delete (e.g., "patch.crates-io")
    #[serde(default)]
    pub delete_sections: Vec<String>,
    /// Workspace members to remove from [workspace] members array
    #[serde(default)]
    pub delete_members: Vec<String>,
    /// Keys to remove from [workspace.dependencies]
    #[serde(default)]
    pub delete_workspace_deps: Vec<String>,
    /// Per-member-crate dep deletions: { "member" = ["dep1", "dep2"] }
    #[serde(default)]
    pub delete_crate_deps: BTreeMap<String, Vec<String>>,
    /// Per-member-crate feature stripping: { "member" = ["feature1"] }
    #[serde(default)]
    pub strip_features: BTreeMap<String, Vec<String>>,
    /// Per-member-crate key blanking: { "member" = { "key" = "[]" } }
    #[serde(default)]
    pub blank_keys: BTreeMap<String, BTreeMap<String, String>>,
}

impl EcosystemConfig {
    /// Get the CI strategy for a specific dependency of a specific crate
    pub fn ci_strategy_for(&self, crate_name: &str, dep_name: &str) -> CiStrategy {
        if let Some(ovr) = self.ci.overrides.get(crate_name) {
            // Check if this dep is explicitly marked for deletion
            if ovr.delete.contains(&dep_name.to_string()) {
                return CiStrategy::Delete;
            }
            // Use crate-level override strategy, or fall back to global default
            ovr.default_strategy.unwrap_or(self.ci.default_strategy)
        } else {
            self.ci.default_strategy
        }
    }

    /// Get GitHub URL for a repo directory
    pub fn github_url_for(&self, repo_dir: &str) -> Option<String> {
        // Check explicit overrides first
        for r in &self.repo {
            if r.dir == repo_dir {
                if r.no_remote {
                    return None;
                }
                if let Some(gh) = &r.github {
                    return Some(format!("https://github.com/{gh}"));
                }
            }
        }
        // Default convention: org/repo_dir_basename
        let basename = Path::new(repo_dir)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(repo_dir);
        Some(format!(
            "https://github.com/{}/{}",
            self.ecosystem.default_github_org, basename
        ))
    }

    /// Resolve all scan roots relative to the ecosystem root
    pub fn scan_roots(&self, ecosystem_root: &Path) -> Vec<PathBuf> {
        let mut roots = vec![ecosystem_root.to_path_buf()];
        for extra in &self.ecosystem.extra_roots {
            let resolved = ecosystem_root.join(extra);
            if let Ok(canonical) = resolved.canonicalize() {
                roots.push(canonical);
            }
        }
        roots
    }
}

/// Load and parse the ecosystem config file
pub fn load_config(path: &Path) -> Result<EcosystemConfig, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    toml::from_str(&content).map_err(|e| format!("parsing {}: {e}", path.display()))
}
