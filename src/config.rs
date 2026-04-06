use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Top-level configuration from Superwork.toml
#[derive(Debug, Deserialize)]
pub struct SuperworkConfig {
    pub superworkspace: SuperworkspaceMeta,
    #[serde(default)]
    pub repo: Vec<RepoOverride>,
    #[serde(default)]
    pub unpublished: UnpublishedConfig,
    #[serde(default)]
    pub ci: CiConfig,
    #[serde(default)]
    pub checks: ChecksConfig,
    #[serde(default)]
    pub release: ReleaseConfig,

    // Backwards compat: accept [ecosystem] as alias for [superworkspace]
    #[serde(default)]
    ecosystem: Option<SuperworkspaceMeta>,
}

impl SuperworkConfig {
    /// Resolve the superworkspace meta, accepting either [superworkspace] or [ecosystem]
    pub fn meta(&self) -> &SuperworkspaceMeta {
        // [superworkspace] takes priority; fall back to [ecosystem] for migration
        if self.superworkspace.name.is_some() || !self.superworkspace.extra_roots.is_empty() {
            &self.superworkspace
        } else if let Some(eco) = &self.ecosystem {
            eco
        } else {
            &self.superworkspace
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct SuperworkspaceMeta {
    /// Display name for this superworkspace
    pub name: Option<String>,
    /// Default GitHub org for URL generation
    #[serde(default = "default_org")]
    pub default_github_org: String,
    /// Additional directories to scan (relative to config file)
    #[serde(default)]
    pub extra_roots: Vec<String>,
    /// GitHub orgs/users that own crates in this superworkspace.
    /// Crates on crates.io whose `repository` URL doesn't match any of these
    /// are considered external forks (use crates.io version, don't publish).
    /// Defaults to [default_github_org].
    #[serde(default)]
    pub owned_orgs: Vec<String>,
}

fn default_org() -> String {
    "imazen".to_string()
}

/// Override for a repo whose GitHub URL differs from convention
#[derive(Debug, Deserialize)]
pub struct RepoOverride {
    /// Directory relative to superworkspace root
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
    #[serde(default)]
    pub crates: BTreeSet<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct CiConfig {
    #[serde(default = "default_ci_strategy")]
    pub default_strategy: CiStrategy,
    #[serde(default)]
    pub overrides: BTreeMap<String, CiCrateOverride>,
}

fn default_ci_strategy() -> CiStrategy {
    CiStrategy::GitUrl
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiStrategy {
    #[default]
    GitUrl,
    StripPath,
    Delete,
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct CiCrateOverride {
    #[serde(alias = "strategy")]
    pub default_strategy: Option<CiStrategy>,
    /// Deps that should use git_url strategy even when default is strip_path
    #[serde(default)]
    pub git_url_override: Vec<String>,
    #[serde(default)]
    pub delete: Vec<String>,
    #[serde(default)]
    pub delete_sections: Vec<String>,
    #[serde(default)]
    pub delete_members: Vec<String>,
    #[serde(default)]
    pub delete_workspace_deps: Vec<String>,
    #[serde(default)]
    pub delete_crate_deps: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub strip_features: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub blank_keys: BTreeMap<String, BTreeMap<String, String>>,
}

/// Named check commands that can be run across repos
#[derive(Debug, Default, Deserialize)]
pub struct ChecksConfig {
    /// Named checks: "test" = "cargo test", "clippy" = "cargo clippy ..."
    #[serde(flatten)]
    pub commands: BTreeMap<String, CheckDef>,
    /// Per-repo overrides for check commands
    #[serde(default)]
    pub repo_overrides: BTreeMap<String, BTreeMap<String, String>>,
}

/// A check can be a simple string command or a detailed definition
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
pub enum CheckDef {
    Simple(String),
    Detailed {
        cmd: String,
        /// Only run on these crates (glob pattern)
        #[serde(default)]
        filter: Option<String>,
        /// Only run on publishable crates
        #[serde(default)]
        only_publishable: bool,
        /// Only run on crates changed since last tag
        #[serde(default)]
        only_changed: bool,
    },
}

impl CheckDef {
    pub fn command(&self) -> &str {
        match self {
            Self::Simple(s) => s,
            Self::Detailed { cmd, .. } => cmd,
        }
    }
}

/// Classification of a crate's release semantics
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CrateClass {
    /// Full semver analysis + coordinated release
    #[default]
    Library,
    /// Independently versioned, no semver-checks needed (CLI tools, etc.)
    Binary,
    /// Independently versioned, ABI stability matters (FFI crates like imageflow_abi)
    Abi,
}

impl std::fmt::Display for CrateClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Library => write!(f, "library"),
            Self::Binary => write!(f, "binary"),
            Self::Abi => write!(f, "abi"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[allow(dead_code)]
pub struct ReleaseConfig {
    /// Checks to run before publishing
    #[serde(default)]
    pub pre_publish: Vec<String>,
    /// Override crate classification (library/binary/abi)
    #[serde(default)]
    pub crate_class: BTreeMap<String, CrateClass>,
    /// Per-crate CI job failure allowlist (crate name → list of job name globs)
    #[serde(default)]
    pub ci_allow_failures: BTreeMap<String, Vec<String>>,
    /// Global CI job failure allowlist (job name globs)
    #[serde(default)]
    pub ci_allow_failures_global: Vec<String>,
    /// Cross-compilation targets for local testing (e.g., "i686-unknown-linux-gnu")
    #[serde(default)]
    pub local_targets: Vec<String>,
    /// Seconds to wait for crates.io index propagation after publish (default: 30)
    #[serde(default = "default_index_wait")]
    pub index_wait_secs: u64,
}

fn default_index_wait() -> u64 {
    30
}

impl SuperworkConfig {
    /// Get CI strategy for a dep, checking inline metadata first, then central config.
    pub fn ci_strategy_for(
        &self,
        crate_name: &str,
        dep_name: &str,
        inline: Option<&CiCrateOverride>,
    ) -> CiStrategy {
        // Inline metadata takes priority
        if let Some(ovr) = inline {
            if ovr.delete.contains(&dep_name.to_string()) {
                return CiStrategy::Delete;
            }
            if ovr.git_url_override.contains(&dep_name.to_string()) {
                return CiStrategy::GitUrl;
            }
            if let Some(s) = ovr.default_strategy {
                return s;
            }
        }
        // Fall back to central config
        if let Some(ovr) = self.ci.overrides.get(crate_name) {
            if ovr.delete.contains(&dep_name.to_string()) {
                return CiStrategy::Delete;
            }
            ovr.default_strategy.unwrap_or(self.ci.default_strategy)
        } else {
            self.ci.default_strategy
        }
    }

    /// Get the merged CI override for a crate (inline + central, inline wins)
    pub fn ci_override_for<'a>(
        &'a self,
        crate_name: &str,
        inline: Option<&'a CiCrateOverride>,
    ) -> Option<MergedCiOverride<'a>> {
        let central = self.ci.overrides.get(crate_name);
        if inline.is_none() && central.is_none() {
            return None;
        }
        Some(MergedCiOverride { inline, central })
    }

    pub fn github_url_for(&self, repo_dir: &str) -> Option<String> {
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
        let basename = Path::new(repo_dir)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(repo_dir);
        Some(format!(
            "https://github.com/{}/{}",
            self.meta().default_github_org,
            basename
        ))
    }

    /// Get the list of GitHub orgs/users that own crates in this superworkspace.
    pub fn owned_orgs(&self) -> Vec<&str> {
        let meta = self.meta();
        if meta.owned_orgs.is_empty() {
            vec![meta.default_github_org.as_str()]
        } else {
            meta.owned_orgs.iter().map(|s| s.as_str()).collect()
        }
    }

    pub fn scan_roots(&self, superworkspace_root: &Path) -> Vec<PathBuf> {
        let mut roots = vec![superworkspace_root.to_path_buf()];
        for extra in &self.meta().extra_roots {
            let resolved = superworkspace_root.join(extra);
            if let Ok(canonical) = resolved.canonicalize() {
                roots.push(canonical);
            }
        }
        roots
    }

    /// Get the command for a named check, with per-repo override support
    pub fn check_command(&self, check_name: &str, repo_dir: &str) -> Option<String> {
        // Check per-repo override first
        if let Some(overrides) = self.checks.repo_overrides.get(repo_dir) {
            if let Some(cmd) = overrides.get(check_name) {
                return Some(cmd.clone());
            }
        }
        // Fall back to global check definition
        self.checks
            .commands
            .get(check_name)
            .map(|d| d.command().to_string())
    }
}

/// Merged view of inline (per-repo) + central CI overrides
pub struct MergedCiOverride<'a> {
    pub inline: Option<&'a CiCrateOverride>,
    pub central: Option<&'a CiCrateOverride>,
}

impl MergedCiOverride<'_> {
    fn pick(&self) -> Option<&CiCrateOverride> {
        self.inline.or(self.central)
    }

    pub fn delete_sections(&self) -> &[String] {
        // Inline wins, fall back to central
        if let Some(ovr) = self.inline {
            if !ovr.delete_sections.is_empty() {
                return &ovr.delete_sections;
            }
        }
        self.central
            .map(|c| c.delete_sections.as_slice())
            .unwrap_or_default()
    }

    pub fn delete_members(&self) -> &[String] {
        self.pick()
            .map(|o| o.delete_members.as_slice())
            .unwrap_or_default()
    }

    pub fn delete_workspace_deps(&self) -> &[String] {
        self.pick()
            .map(|o| o.delete_workspace_deps.as_slice())
            .unwrap_or_default()
    }

    pub fn delete_crate_deps(&self) -> &BTreeMap<String, Vec<String>> {
        static EMPTY: std::sync::LazyLock<BTreeMap<String, Vec<String>>> =
            std::sync::LazyLock::new(BTreeMap::new);
        self.pick().map(|o| &o.delete_crate_deps).unwrap_or(&EMPTY)
    }

    pub fn strip_features(&self) -> &BTreeMap<String, Vec<String>> {
        static EMPTY: std::sync::LazyLock<BTreeMap<String, Vec<String>>> =
            std::sync::LazyLock::new(BTreeMap::new);
        self.pick().map(|o| &o.strip_features).unwrap_or(&EMPTY)
    }

    pub fn blank_keys(&self) -> &BTreeMap<String, BTreeMap<String, String>> {
        static EMPTY: std::sync::LazyLock<BTreeMap<String, BTreeMap<String, String>>> =
            std::sync::LazyLock::new(BTreeMap::new);
        self.pick().map(|o| &o.blank_keys).unwrap_or(&EMPTY)
    }
}

pub fn load_config(path: &Path) -> Result<SuperworkConfig, String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    toml::from_str(&content).map_err(|e| format!("parsing {}: {e}", path.display()))
}
