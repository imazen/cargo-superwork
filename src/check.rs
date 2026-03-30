use crate::config::SuperworkConfig;
use crate::discover;
use std::path::Path;

pub fn run(ecosystem_root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;

    let mut errors = 0;
    let mut warnings = 0;

    println!("=== Ecosystem Check ===");
    println!();

    // 1. Check dual-spec: publishable deps must have both path and version
    println!("--- Dual-spec Enforcement ---");
    for dep in &eco.deps {
        let from_publishable = eco
            .crates
            .get(&dep.from_crate)
            .is_some_and(|c| c.publishable);
        let to_publishable = eco.crates.get(&dep.to_crate).is_some_and(|c| c.publishable);

        if from_publishable && to_publishable && dep.has_path && !dep.has_version {
            eprintln!(
                "  ERROR: {} -> {} ({}): path-only, needs version for publish",
                dep.from_crate, dep.to_crate, dep.section
            );
            errors += 1;
        } else if dep.has_path && !dep.has_version && !to_publishable {
            // Path-only to unpublished crate is OK but worth noting
            warnings += 1;
        }
    }

    // 2. Check version consistency
    println!();
    println!("--- Version Consistency ---");
    for dep in &eco.deps {
        if let (Some(version_req_str), Some(crate_info)) =
            (&dep.version_value, eco.crates.get(&dep.to_crate))
        {
            // Parse as semver requirement
            match semver::VersionReq::parse(version_req_str) {
                Ok(req) => {
                    match semver::Version::parse(&crate_info.version) {
                        Ok(ver) => {
                            if !req.matches(&ver) {
                                eprintln!(
                                    "  ERROR: {} -> {} requires \"{}\", but actual version is {}",
                                    dep.from_crate,
                                    dep.to_crate,
                                    version_req_str,
                                    crate_info.version
                                );
                                errors += 1;
                            }
                        }
                        Err(_) => {
                            // Non-semver version, skip
                        }
                    }
                }
                Err(_) => {
                    // May be a non-standard version spec, skip
                }
            }
        }
    }

    // 3. Check path validity
    println!();
    println!("--- Path Validity ---");
    for dep in &eco.deps {
        if let Some(path_str) = &dep.path_value {
            let manifest_dir = dep.manifest_path.parent().unwrap();
            let resolved = manifest_dir.join(path_str);
            let cargo_toml = resolved.join("Cargo.toml");
            if !cargo_toml.exists() {
                eprintln!(
                    "  ERROR: {} -> {}: path \"{}\" does not exist (from {})",
                    dep.from_crate,
                    dep.to_crate,
                    path_str,
                    dep.manifest_path.display()
                );
                errors += 1;
            }
        }
    }

    // 4. Check for crates not in registry but referenced via path
    println!();
    println!("--- Unknown References ---");
    for dep in &eco.deps {
        if !eco.crates.contains_key(&dep.to_crate) {
            eprintln!(
                "  WARNING: {} -> {}: not found in ecosystem registry",
                dep.from_crate, dep.to_crate
            );
            warnings += 1;
        }
    }

    // Summary
    println!();
    println!("=== Summary ===");
    println!(
        "  {} crates, {} internal deps",
        eco.crates.len(),
        eco.deps.len()
    );
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
    println!("  {dual} dual-specified, {path_only} path-only");
    println!("  {errors} errors, {warnings} warnings");

    if errors > 0 {
        Err(format!("{errors} errors found"))
    } else {
        Ok(())
    }
}
