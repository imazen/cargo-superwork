//! Generate and sync CI workflow files across all ecosystem repos.
//!
//! Reads a template from the workspace directory and writes it to each repo's
//! `.github/workflows/ci.yml`, substituting per-repo variables.

use crate::config::SuperworkConfig;
use crate::discover;
use std::collections::BTreeMap;
use std::path::Path;

const DEFAULT_TEMPLATE_NAME: &str = "ci-template.yml";
const DEFAULT_WORKFLOW_PATH: &str = ".github/workflows/ci.yml";

pub fn run(
    root: &Path,
    config: &SuperworkConfig,
    template_path: Option<&str>,
    filter: Option<&str>,
    dry_run: bool,
) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;

    // Load template
    let tmpl_path = if let Some(p) = template_path {
        root.join(p)
    } else {
        root.join(DEFAULT_TEMPLATE_NAME)
    };

    if !tmpl_path.exists() {
        // Generate a default template
        if dry_run {
            println!(
                "No template found at {}. Would generate default.",
                tmpl_path.display()
            );
            println!("Create the template first, or run without --dry-run to generate a default.");
            return Ok(());
        }
        generate_default_template(&tmpl_path, config)?;
        println!("Generated default template at {}", tmpl_path.display());
        println!("Edit it, then re-run to apply to all repos.");
        return Ok(());
    }

    let template = std::fs::read_to_string(&tmpl_path)
        .map_err(|e| format!("reading template {}: {e}", tmpl_path.display()))?;

    // Group crates by repo
    let mut repos: BTreeMap<&str, Vec<&discover::CrateInfo>> = BTreeMap::new();
    for info in eco.crates.values() {
        repos.entry(&info.repo_dir).or_default().push(info);
    }

    let label = if dry_run { "[dry-run] " } else { "" };
    let mut updated = 0;
    let mut skipped = 0;

    for (repo_dir, crates) in &repos {
        if let Some(f) = filter {
            if !crates.iter().any(|c| glob_match(f, &c.name)) {
                continue;
            }
        }

        let repo_path = root.join(repo_dir);
        if !repo_path.exists() {
            continue;
        }

        // Check for opt-out via [package.metadata.superwork.ci] skip_ci_gen = true
        let opted_out = crates.iter().any(|c| {
            c.inline_ci.as_ref().is_some_and(|ci| {
                // Check if any field indicates opt-out — for now, check delete_sections
                // contains "ci-gen-skip" as a convention
                ci.delete_sections.iter().any(|s| s == "ci-gen-skip")
            })
        });
        if opted_out {
            println!("{label}  skip: {repo_dir} (opted out)");
            skipped += 1;
            continue;
        }

        // Determine per-repo variables for substitution
        let primary_crate = crates.iter().find(|c| c.publishable).or(crates.first());
        let crate_name = primary_crate.map(|c| c.name.as_str()).unwrap_or(repo_dir);
        let msrv = "1.85"; // Could be extracted from Cargo.toml rust-version

        let rendered = template
            .replace("{{crate_name}}", crate_name)
            .replace("{{repo_dir}}", repo_dir)
            .replace("{{msrv}}", msrv);

        let workflow_path = repo_path.join(DEFAULT_WORKFLOW_PATH);

        // Check if content changed
        let existing = std::fs::read_to_string(&workflow_path).unwrap_or_default();
        if existing == rendered {
            skipped += 1;
            continue;
        }

        println!("{label}  write: {repo_dir}/{DEFAULT_WORKFLOW_PATH}");

        if !dry_run {
            if let Some(parent) = workflow_path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            }
            std::fs::write(&workflow_path, &rendered)
                .map_err(|e| format!("writing {}: {e}", workflow_path.display()))?;
        }

        updated += 1;
    }

    println!();
    println!(
        "{label}{updated} updated, {skipped} unchanged/skipped (of {} repos)",
        repos.len()
    );
    Ok(())
}

fn generate_default_template(path: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let _org = &config.meta().default_github_org;
    let template = r##"name: CI

on:
  push:
    branches: [main, master]
  pull_request:
    branches: [main, master]

env:
  CARGO_TERM_COLOR: always
  RUST_BACKTRACE: 1

jobs:
  test:
    name: Test (${{{{ matrix.name }}}})
    runs-on: ${{{{ matrix.os }}}}
    strategy:
      fail-fast: false
      matrix:
        include:
          - {{ name: linux-x64, os: ubuntu-latest }}
          - {{ name: linux-arm64, os: ubuntu-24.04-arm }}
          - {{ name: macos-arm64, os: macos-latest }}
          - {{ name: macos-x64, os: macos-15-intel }}
          - {{ name: windows-x64, os: windows-latest }}
          - {{ name: windows-arm64, os: windows-11-arm }}
          - {{ name: linux-i686, os: ubuntu-latest, target: i686-unknown-linux-gnu, cross: true }}
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - name: Install cross
        if: matrix.cross
        run: cargo install cross --git https://github.com/cross-rs/cross
      - name: cargo test
        if: '!matrix.cross'
        run: cargo test
      - name: cross test
        if: matrix.cross
        run: cross test --target ${{{{ matrix.target }}}}

  clippy:
    name: Clippy
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - name: cargo clippy
        run: cargo clippy --all-targets -- -D warnings

  fmt:
    name: Format
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt
      - name: cargo fmt
        run: cargo fmt -- --check
"##;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("creating {}: {e}", parent.display()))?;
    }
    std::fs::write(path, template).map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(())
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
    pattern == name
}
