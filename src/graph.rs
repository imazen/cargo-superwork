use crate::discover::Ecosystem;
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::BTreeMap;

/// Build a dependency graph from the ecosystem.
/// Returns (graph, node_index_by_crate_name).
#[allow(dead_code)]
pub fn build_graph(eco: &Ecosystem) -> (DiGraph<String, ()>, BTreeMap<String, NodeIndex>) {
    let mut graph = DiGraph::new();
    let mut indices: BTreeMap<String, NodeIndex> = BTreeMap::new();

    // Add nodes for all crates
    for name in eco.crates.keys() {
        let idx = graph.add_node(name.clone());
        indices.insert(name.clone(), idx);
    }

    // Add edges: from_crate depends on to_crate → edge from from_crate to to_crate
    for dep in &eco.deps {
        if let (Some(&from_idx), Some(&to_idx)) =
            (indices.get(&dep.from_crate), indices.get(&dep.to_crate))
        {
            // Avoid duplicate edges
            if !graph.contains_edge(from_idx, to_idx) {
                graph.add_edge(from_idx, to_idx, ());
            }
        }
    }

    (graph, indices)
}

/// Compute topological publish order.
/// Returns levels: crates at the same level can be published in parallel.
/// Level 0 = leaf crates (no internal deps), Level N = depends on level N-1.
pub fn publish_order(eco: &Ecosystem, publishable_only: bool) -> Result<Vec<Vec<String>>, String> {
    let mut graph = DiGraph::new();
    let mut indices: BTreeMap<String, NodeIndex> = BTreeMap::new();

    // Add nodes
    for (name, info) in &eco.crates {
        if publishable_only && !info.publishable {
            continue;
        }
        let idx = graph.add_node(name.clone());
        indices.insert(name.clone(), idx);
    }

    // Add edges (skip dev-deps and self-deps — they don't affect publish order)
    for dep in &eco.deps {
        if dep.section == crate::discover::DepSection::DevDependencies {
            continue;
        }
        if dep.from_crate == dep.to_crate {
            continue;
        }
        if let (Some(&from_idx), Some(&to_idx)) =
            (indices.get(&dep.from_crate), indices.get(&dep.to_crate))
        {
            if !graph.contains_edge(from_idx, to_idx) {
                graph.add_edge(from_idx, to_idx, ());
            }
        }
    }

    // Topological sort
    let sorted = toposort(&graph, None).map_err(|cycle| {
        let crate_name = &graph[cycle.node_id()];
        format!("circular dependency detected involving '{crate_name}'")
    })?;

    // Assign levels based on longest path from leaves
    let mut levels: BTreeMap<NodeIndex, usize> = BTreeMap::new();

    // Reverse topological order (process leaves first)
    for &node in sorted.iter().rev() {
        let max_dep_level = graph
            .neighbors(node)
            .map(|dep| levels.get(&dep).copied().unwrap_or(0))
            .max();

        let level = match max_dep_level {
            Some(l) => l + 1,
            None => 0,
        };
        levels.insert(node, level);
    }

    // Group by level
    let max_level = levels.values().max().copied().unwrap_or(0);
    let mut result = vec![Vec::new(); max_level + 1];

    for (node, level) in &levels {
        result[*level].push(graph[*node].clone());
    }

    // Sort within each level for deterministic output
    for level in &mut result {
        level.sort();
    }

    Ok(result)
}
