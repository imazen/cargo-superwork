//! Format-preserving Cargo.toml manipulation using toml_edit.
//!
//! All operations read, modify, and write back Cargo.toml files
//! while preserving comments, formatting, and key ordering.

use std::path::Path;
use toml_edit::DocumentMut;

/// Read and parse a Cargo.toml file
pub fn read_manifest(path: &Path) -> Result<(String, DocumentMut), String> {
    let content =
        std::fs::read_to_string(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let doc: DocumentMut = content
        .parse()
        .map_err(|e| format!("parsing {}: {e}", path.display()))?;
    Ok((content, doc))
}

/// Write a manifest back to disk
pub fn write_manifest(path: &Path, doc: &DocumentMut, dry_run: bool) -> Result<bool, String> {
    let new_content = doc.to_string();

    // Read existing content to check if anything changed
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    if existing == new_content {
        return Ok(false);
    }

    if dry_run {
        return Ok(true);
    }

    std::fs::write(path, new_content.as_bytes())
        .map_err(|e| format!("writing {}: {e}", path.display()))?;
    Ok(true)
}

/// Remove the `path` key from a dependency entry in a specific section.
/// Returns true if the key was found and removed.
pub fn remove_dep_path(doc: &mut DocumentMut, section: &str, dep_name: &str) -> bool {
    let Some(deps) = doc.get_mut(section).and_then(|s| s.as_table_like_mut()) else {
        return false;
    };
    let Some(dep) = deps.get_mut(dep_name) else {
        return false;
    };

    if let Some(tbl) = dep.as_inline_table_mut() {
        if tbl.contains_key("path") {
            tbl.remove("path");
            return true;
        }
    } else if let Some(tbl) = dep.as_table_mut() {
        if tbl.contains_key("path") {
            tbl.remove("path");
            return true;
        }
    }
    false
}

/// Replace the `path` key with a `git` key in a dependency entry.
/// Returns true if the replacement was made.
pub fn replace_path_with_git(
    doc: &mut DocumentMut,
    section: &str,
    dep_name: &str,
    git_url: &str,
) -> bool {
    let Some(deps) = doc.get_mut(section).and_then(|s| s.as_table_like_mut()) else {
        return false;
    };
    let Some(dep) = deps.get_mut(dep_name) else {
        return false;
    };

    if let Some(tbl) = dep.as_inline_table_mut() {
        if tbl.contains_key("path") {
            tbl.remove("path");
            tbl.insert("git", git_url.into());
            return true;
        }
    } else if let Some(tbl) = dep.as_table_mut() {
        if tbl.contains_key("path") {
            tbl.remove("path");
            tbl.insert("git", toml_edit::value(git_url));
            return true;
        }
    }
    false
}

/// Add or update a `path` key on a dependency entry.
/// Returns true if a change was made.
pub fn set_dep_path(doc: &mut DocumentMut, section: &str, dep_name: &str, path: &str) -> bool {
    let Some(deps) = doc.get_mut(section).and_then(|s| s.as_table_like_mut()) else {
        return false;
    };
    let Some(dep) = deps.get_mut(dep_name) else {
        return false;
    };

    if let Some(tbl) = dep.as_inline_table_mut() {
        let existing = tbl.get("path").and_then(|v| v.as_str()).map(String::from);
        if existing.as_deref() == Some(path) {
            return false;
        }
        tbl.insert("path", path.into());
        return true;
    } else if let Some(tbl) = dep.as_table_mut() {
        let existing = tbl.get("path").and_then(|v| v.as_str()).map(String::from);
        if existing.as_deref() == Some(path) {
            return false;
        }
        tbl.insert("path", toml_edit::value(path));
        return true;
    } else if dep.as_str().is_some() {
        // Currently a bare string like `dep = "0.1"`. Convert to inline table.
        let version_str = dep.as_str().unwrap().to_string();
        let mut tbl = toml_edit::InlineTable::new();
        tbl.insert("version", version_str.as_str().into());
        tbl.insert("path", path.into());
        *dep = toml_edit::Item::Value(toml_edit::Value::InlineTable(tbl));
        return true;
    }
    false
}

/// Delete an entire dependency entry from a section.
/// Returns true if the entry was found and removed.
pub fn delete_dep(doc: &mut DocumentMut, section: &str, dep_name: &str) -> bool {
    let Some(deps) = doc.get_mut(section).and_then(|s| s.as_table_like_mut()) else {
        return false;
    };
    if deps.contains_key(dep_name) {
        deps.remove(dep_name);
        return true;
    }
    false
}

/// Delete a TOML section (e.g., "patch.crates-io").
/// Handles dotted section paths like "patch.crates-io".
/// Returns true if the section was found and removed.
pub fn delete_section(doc: &mut DocumentMut, section_path: &str) -> bool {
    let parts: Vec<&str> = section_path.split('.').collect();

    match parts.len() {
        1 => {
            if doc.contains_key(parts[0]) {
                doc.remove(parts[0]);
                return true;
            }
        }
        2 => {
            if let Some(parent) = doc.get_mut(parts[0]).and_then(|s| s.as_table_mut()) {
                if parent.contains_key(parts[1]) {
                    parent.remove(parts[1]);
                    // Remove parent if now empty
                    let is_empty = parent.is_empty();
                    if is_empty {
                        doc.remove(parts[0]);
                    }
                    return true;
                }
            }
        }
        _ => {} // Deeper nesting not supported
    }
    false
}

/// Remove a member from the [workspace] members array.
/// Returns true if the member was found and removed.
pub fn remove_workspace_member(doc: &mut DocumentMut, member: &str) -> bool {
    let Some(workspace) = doc.get_mut("workspace").and_then(|w| w.as_table_mut()) else {
        return false;
    };
    let Some(members) = workspace.get_mut("members").and_then(|m| m.as_array_mut()) else {
        return false;
    };

    let initial_len = members.len();
    members.retain(|v| v.as_str() != Some(member));
    members.len() < initial_len
}

/// Remove a key from [workspace.dependencies].
/// Returns true if the key was found and removed.
pub fn remove_workspace_dep(doc: &mut DocumentMut, dep_name: &str) -> bool {
    let Some(workspace) = doc.get_mut("workspace").and_then(|w| w.as_table_mut()) else {
        return false;
    };
    let Some(deps) = workspace
        .get_mut("dependencies")
        .and_then(|d| d.as_table_like_mut())
    else {
        return false;
    };
    if deps.contains_key(dep_name) {
        deps.remove(dep_name);
        return true;
    }
    false
}

/// Strip a specific feature from a dependency's features list.
/// Returns true if the feature was found and removed.
pub fn strip_dep_feature(
    doc: &mut DocumentMut,
    section: &str,
    dep_name: &str,
    feature: &str,
) -> bool {
    let Some(deps) = doc.get_mut(section).and_then(|s| s.as_table_like_mut()) else {
        return false;
    };
    let Some(dep) = deps.get_mut(dep_name) else {
        return false;
    };

    let features_arr = if let Some(tbl) = dep.as_inline_table_mut() {
        tbl.get_mut("features").and_then(|f| f.as_array_mut())
    } else if let Some(tbl) = dep.as_table_mut() {
        tbl.get_mut("features")
            .and_then(|f| f.as_value_mut())
            .and_then(|v| v.as_array_mut())
    } else {
        None
    };

    if let Some(arr) = features_arr {
        let initial_len = arr.len();
        arr.retain(|v| v.as_str() != Some(feature));
        return arr.len() < initial_len;
    }
    false
}

/// Set a dependency key to a literal string value (e.g., blank it to "[]").
/// Used for the zenjpeg CI override where layout = [] and zennode = [].
pub fn set_dep_value_raw(
    doc: &mut DocumentMut,
    section: &str,
    dep_name: &str,
    raw_value: &str,
) -> bool {
    let Some(deps) = doc.get_mut(section).and_then(|s| s.as_table_like_mut()) else {
        return false;
    };
    if !deps.contains_key(dep_name) {
        return false;
    }

    // Parse the raw value as a TOML value
    if raw_value == "[]" {
        let arr = toml_edit::Array::new();
        deps.insert(
            dep_name,
            toml_edit::Item::Value(toml_edit::Value::Array(arr)),
        );
        return true;
    }

    // Try parsing as a general TOML value
    if let Ok(val) = raw_value.parse::<toml_edit::Value>() {
        deps.insert(dep_name, toml_edit::Item::Value(val));
        return true;
    }

    false
}

/// Update the version string in [package].version
pub fn set_package_version(doc: &mut DocumentMut, version: &str) -> bool {
    if let Some(pkg) = doc.get_mut("package").and_then(|p| p.as_table_mut()) {
        let existing = pkg
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        if existing.as_deref() == Some(version) {
            return false;
        }
        pkg.insert("version", toml_edit::value(version));
        return true;
    }
    false
}

/// Update the version in a dependency entry (in any section).
pub fn set_dep_version(
    doc: &mut DocumentMut,
    section: &str,
    dep_name: &str,
    version: &str,
) -> bool {
    let Some(deps) = doc.get_mut(section).and_then(|s| s.as_table_like_mut()) else {
        return false;
    };
    let Some(dep) = deps.get_mut(dep_name) else {
        return false;
    };

    if let Some(tbl) = dep.as_inline_table_mut() {
        let existing = tbl
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        if existing.as_deref() == Some(version) {
            return false;
        }
        tbl.insert("version", version.into());
        return true;
    } else if let Some(tbl) = dep.as_table_mut() {
        let existing = tbl
            .get("version")
            .and_then(|v| v.as_str())
            .map(String::from);
        if existing.as_deref() == Some(version) {
            return false;
        }
        tbl.insert("version", toml_edit::value(version));
        return true;
    } else if dep.as_str().is_some() {
        // Bare string version
        let existing = dep.as_str().map(String::from);
        if existing.as_deref() == Some(version) {
            return false;
        }
        *dep = toml_edit::Item::Value(version.into());
        return true;
    }
    false
}
