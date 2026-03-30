use crate::config::SuperworkConfig;
use crate::discover;
use crate::graph;
use std::path::Path;

pub fn run(ecosystem_root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(ecosystem_root, config)?;
    let levels = graph::publish_order(&eco, true)?;

    println!(
        "=== Publish Order ({} publishable crates) ===",
        levels.iter().map(|l| l.len()).sum::<usize>()
    );
    println!();

    for (i, level) in levels.iter().enumerate() {
        if level.is_empty() {
            continue;
        }
        println!("Level {i} ({} crates):", level.len());
        for name in level {
            let version = eco
                .crates
                .get(name)
                .map(|c| c.version.as_str())
                .unwrap_or("?");
            println!("  {name} {version}");
        }
        println!();
    }

    // Check for publishability blockers
    let mut blockers = 0;
    for dep in &eco.deps {
        let from_pub = eco
            .crates
            .get(&dep.from_crate)
            .is_some_and(|c| c.publishable);
        let to_pub = eco.crates.get(&dep.to_crate).is_some_and(|c| c.publishable);
        if from_pub && to_pub && dep.has_path && !dep.has_version {
            if blockers == 0 {
                println!("=== Publish Blockers (path-only deps) ===");
            }
            println!("  {} -> {}: needs version", dep.from_crate, dep.to_crate);
            blockers += 1;
        }
    }

    if blockers > 0 {
        println!();
        println!("{blockers} path-only deps must be dual-specified before publishing.");
    }

    Ok(())
}
