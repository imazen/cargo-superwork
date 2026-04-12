use crate::config::SuperworkConfig;
use crate::discover;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

/// Maximum repos per GraphQL request (GitHub node limits).
const BATCH_SIZE: usize = 40;

struct RepoSlug {
    /// GraphQL alias (sanitized for identifier rules)
    alias: String,
    /// Display name (e.g. "zenwebp")
    display: String,
    /// owner/repo slug (e.g. "imazen/zenwebp")
    slug: String,
}

struct RepoResult {
    display: String,
    ci_state: Option<String>,
    pr_count: u64,
    issue_count: u64,
    prs: Vec<PrInfo>,
    issues: Vec<IssueInfo>,
}

struct PrInfo {
    repo: String,
    number: u64,
    branch: String,
    title: String,
    author: String,
}

struct IssueInfo {
    repo: String,
    number: u64,
    title: String,
}

pub fn run(root: &Path, config: &SuperworkConfig) -> Result<(), String> {
    let eco = discover::scan_ecosystem(root, config)?;

    // Collect unique repo_dir → github slug mappings
    let mut seen = BTreeMap::new();
    for info in eco.crates.values() {
        if seen.contains_key(&info.repo_dir) {
            continue;
        }
        if let Some(url) = config.github_url_for(&info.repo_dir) {
            if let Some(slug) = url
                .strip_prefix("https://github.com/")
                .map(|s| s.trim_end_matches(".git").to_string())
            {
                let display = Path::new(&info.repo_dir)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&info.repo_dir)
                    .to_string();
                let alias = sanitize_alias(&display);
                seen.insert(
                    info.repo_dir.clone(),
                    RepoSlug {
                        alias,
                        display,
                        slug,
                    },
                );
            }
        }
    }

    let slugs: Vec<RepoSlug> = seen.into_values().collect();

    if slugs.is_empty() {
        println!("No repos with GitHub URLs found.");
        return Ok(());
    }

    // Execute in batches
    let mut all_results: Vec<RepoResult> = Vec::new();

    for batch in slugs.chunks(BATCH_SIZE) {
        let query = build_query(batch);
        let json = execute_graphql(&query)?;
        parse_results(&json, batch, &mut all_results)?;
    }

    // Sort alphabetically
    all_results.sort_by(|a, b| a.display.cmp(&b.display));

    // Print table
    let name_w = all_results
        .iter()
        .map(|r| r.display.len())
        .max()
        .unwrap_or(4)
        .max(4)
        + 1;

    println!(
        "{:<name_w$}  {:<4}  {:<4}  {:<6}",
        "Repo", "CI", "PRs", "Issues"
    );
    println!("{}", "-".repeat(name_w + 20));

    let mut ci_green = 0u32;
    let mut ci_red = 0u32;
    let mut total_prs = 0u64;
    let mut total_issues = 0u64;
    let mut all_prs: Vec<&PrInfo> = Vec::new();
    let mut all_issues: Vec<&IssueInfo> = Vec::new();

    for r in &all_results {
        let ci_icon = match r.ci_state.as_deref() {
            Some("SUCCESS") => {
                ci_green += 1;
                "\u{2705}" // check mark
            }
            Some("PENDING") | Some("EXPECTED") => "\u{23f3}", // hourglass
            Some("FAILURE") | Some("ERROR") => {
                ci_red += 1;
                "\u{274c}" // X
            }
            None => "\u{2014}", // em dash (no CI)
            Some(_) => {
                ci_red += 1;
                "\u{274c}"
            }
        };

        let pr_str = if r.pr_count > 0 {
            r.pr_count.to_string()
        } else {
            "-".to_string()
        };
        let issue_str = if r.issue_count > 0 {
            r.issue_count.to_string()
        } else {
            "-".to_string()
        };

        total_prs += r.pr_count;
        total_issues += r.issue_count;

        for pr in &r.prs {
            all_prs.push(pr);
        }
        for issue in &r.issues {
            all_issues.push(issue);
        }

        println!(
            "{:<name_w$}  {:<4}  {:<4}  {:<6}",
            r.display, ci_icon, pr_str, issue_str
        );
    }

    // Open PRs section
    if !all_prs.is_empty() {
        println!();
        println!("Open PRs ({total_prs}):");
        for pr in &all_prs {
            println!(
                "  {}#{} [{}] {} (@{})",
                pr.repo, pr.number, pr.branch, pr.title, pr.author
            );
        }
    }

    // Open Issues section
    if !all_issues.is_empty() {
        println!();
        println!("Open Issues ({total_issues}):");
        for issue in &all_issues {
            println!("  {}#{} {}", issue.repo, issue.number, issue.title);
        }
    }

    // Summary
    println!();
    println!(
        "{} repos  |  {} green, {} red  |  {} PRs  |  {} issues",
        all_results.len(),
        ci_green,
        ci_red,
        total_prs,
        total_issues
    );

    Ok(())
}

/// Sanitize a repo name into a valid GraphQL alias.
/// Replace `-`, `.` with `_`, prefix with `r` if starts with digit.
fn sanitize_alias(name: &str) -> String {
    let mut alias: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();
    if alias.starts_with(|c: char| c.is_ascii_digit()) {
        alias.insert(0, 'r');
    }
    // Ensure uniqueness is handled by caller if needed; for our purposes repo names
    // within a single superworkspace are unique directories.
    alias
}

fn build_query(repos: &[RepoSlug]) -> String {
    let fragment = r#"fragment F on Repository {
  nameWithOwner
  defaultBranchRef {
    name
    target {
      ... on Commit {
        statusCheckRollup {
          state
        }
      }
    }
  }
  pullRequests(first: 5, states: OPEN, orderBy: {field: UPDATED_AT, direction: DESC}) {
    totalCount
    nodes {
      number
      title
      author { login }
      updatedAt
      headRefName
    }
  }
  issues(first: 5, states: OPEN, orderBy: {field: UPDATED_AT, direction: DESC}) {
    totalCount
    nodes {
      number
      title
      updatedAt
    }
  }
}"#;

    let mut fields = String::new();
    for repo in repos {
        let parts: Vec<&str> = repo.slug.splitn(2, '/').collect();
        if parts.len() != 2 {
            continue;
        }
        let owner = parts[0];
        let name = parts[1];
        fields.push_str(&format!(
            "  {}: repository(owner: \"{}\", name: \"{}\") {{ ...F }}\n",
            repo.alias, owner, name
        ));
    }

    format!("{fragment}\n{{\n{fields}}}")
}

fn execute_graphql(query: &str) -> Result<String, String> {
    let output = Command::new("gh")
        .args(["api", "graphql", "-f", &format!("query={query}")])
        .output()
        .map_err(|e| format!("failed to run `gh`: {e}"))?;

    let stdout =
        String::from_utf8(output.stdout).map_err(|e| format!("invalid UTF-8 from gh: {e}"))?;

    // GitHub GraphQL returns partial data with errors for non-existent repos.
    // `gh` exits non-zero in this case but stdout still contains valid JSON
    // with null entries for missing repos. Only fail if there's no stdout at all.
    if !output.status.success() && stdout.trim().is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("gh api graphql failed: {stderr}"));
    }

    Ok(stdout)
}

fn parse_results(
    json_str: &str,
    repos: &[RepoSlug],
    results: &mut Vec<RepoResult>,
) -> Result<(), String> {
    // Minimal JSON parsing without serde_json: we use a simple recursive descent
    // approach on the known structure.
    //
    // Actually, the GraphQL response can be complex. Let's do lightweight parsing
    // using string scanning since we don't have serde_json.
    let data = parse_json_value(json_str.trim()).map_err(|e| format!("JSON parse error: {e}"))?;

    let data_obj = json_get(&data, "data");

    for repo in repos {
        let repo_data = json_get(&data_obj, &repo.alias);

        if matches!(repo_data, JsonValue::Null) {
            results.push(RepoResult {
                display: repo.display.clone(),
                ci_state: None,
                pr_count: 0,
                issue_count: 0,
                prs: Vec::new(),
                issues: Vec::new(),
            });
            continue;
        }

        // CI state
        let ci_state = json_get(&repo_data, "defaultBranchRef");
        let ci_state = if matches!(ci_state, JsonValue::Null) {
            None
        } else {
            let target = json_get(&ci_state, "target");
            let rollup = json_get(&target, "statusCheckRollup");
            match json_get(&rollup, "state") {
                JsonValue::Str(s) => Some(s),
                _ => None,
            }
        };

        // PRs
        let prs_obj = json_get(&repo_data, "pullRequests");
        let pr_count = match json_get(&prs_obj, "totalCount") {
            JsonValue::Num(n) => n as u64,
            _ => 0,
        };
        let pr_nodes = json_get_array(&prs_obj, "nodes");
        let mut prs = Vec::new();
        for node in &pr_nodes {
            let number = match json_get(node, "number") {
                JsonValue::Num(n) => n as u64,
                _ => 0,
            };
            let title = match json_get(node, "title") {
                JsonValue::Str(s) => s,
                _ => String::new(),
            };
            let branch = match json_get(node, "headRefName") {
                JsonValue::Str(s) => s,
                _ => String::new(),
            };
            let author_obj = json_get(node, "author");
            let author = match json_get(&author_obj, "login") {
                JsonValue::Str(s) => s,
                _ => "?".to_string(),
            };
            prs.push(PrInfo {
                repo: repo.display.clone(),
                number,
                branch,
                title,
                author,
            });
        }

        // Issues
        let issues_obj = json_get(&repo_data, "issues");
        let issue_count = match json_get(&issues_obj, "totalCount") {
            JsonValue::Num(n) => n as u64,
            _ => 0,
        };
        let issue_nodes = json_get_array(&issues_obj, "nodes");
        let mut issues = Vec::new();
        for node in &issue_nodes {
            let number = match json_get(node, "number") {
                JsonValue::Num(n) => n as u64,
                _ => 0,
            };
            let title = match json_get(node, "title") {
                JsonValue::Str(s) => s,
                _ => String::new(),
            };
            issues.push(IssueInfo {
                repo: repo.display.clone(),
                number,
                title,
            });
        }

        results.push(RepoResult {
            display: repo.display.clone(),
            ci_state,
            pr_count,
            issue_count,
            prs,
            issues,
        });
    }

    Ok(())
}

// ── Minimal JSON parser ──────────────────────────────────────────────────────
// We avoid adding serde_json as a dependency. The GraphQL response has a known
// structure so a simple recursive descent parser suffices.

#[derive(Debug, Clone)]
#[allow(dead_code)]
enum JsonValue {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

fn json_get(val: &JsonValue, key: &str) -> JsonValue {
    match val {
        JsonValue::Object(pairs) => {
            for (k, v) in pairs {
                if k == key {
                    return v.clone();
                }
            }
            JsonValue::Null
        }
        _ => JsonValue::Null,
    }
}

fn json_get_array(val: &JsonValue, key: &str) -> Vec<JsonValue> {
    match json_get(val, key) {
        JsonValue::Array(arr) => arr,
        _ => Vec::new(),
    }
}

fn parse_json_value(input: &str) -> Result<JsonValue, String> {
    let (val, _) = parse_value(input, 0)?;
    Ok(val)
}

fn skip_ws(input: &str, mut pos: usize) -> usize {
    let bytes = input.as_bytes();
    while pos < bytes.len() && matches!(bytes[pos], b' ' | b'\t' | b'\n' | b'\r') {
        pos += 1;
    }
    pos
}

fn parse_value(input: &str, pos: usize) -> Result<(JsonValue, usize), String> {
    let pos = skip_ws(input, pos);
    if pos >= input.len() {
        return Err("unexpected end of input".to_string());
    }
    let b = input.as_bytes()[pos];
    match b {
        b'"' => parse_string(input, pos).map(|(s, p)| (JsonValue::Str(s), p)),
        b'{' => parse_object(input, pos),
        b'[' => parse_array(input, pos),
        b't' | b'f' => parse_bool(input, pos),
        b'n' => parse_null(input, pos),
        b'-' | b'0'..=b'9' => parse_number(input, pos),
        _ => Err(format!("unexpected char '{}' at position {pos}", b as char)),
    }
}

fn parse_string(input: &str, pos: usize) -> Result<(String, usize), String> {
    // pos is at the opening quote
    let mut i = pos + 1;
    let bytes = input.as_bytes();
    let mut result = String::new();
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Ok((result, i + 1)),
            b'\\' => {
                i += 1;
                if i >= bytes.len() {
                    return Err("unterminated string escape".to_string());
                }
                match bytes[i] {
                    b'"' => result.push('"'),
                    b'\\' => result.push('\\'),
                    b'/' => result.push('/'),
                    b'n' => result.push('\n'),
                    b'r' => result.push('\r'),
                    b't' => result.push('\t'),
                    b'b' => result.push('\u{0008}'),
                    b'f' => result.push('\u{000C}'),
                    b'u' => {
                        // \uXXXX
                        if i + 4 >= bytes.len() {
                            return Err("incomplete unicode escape".to_string());
                        }
                        let hex = &input[i + 1..i + 5];
                        let cp = u32::from_str_radix(hex, 16)
                            .map_err(|_| format!("invalid unicode escape: \\u{hex}"))?;
                        if let Some(c) = char::from_u32(cp) {
                            result.push(c);
                        }
                        i += 4;
                    }
                    _ => {
                        result.push('\\');
                        result.push(bytes[i] as char);
                    }
                }
                i += 1;
            }
            _ => {
                // UTF-8 safe: get the char at this byte position
                let ch = input[i..].chars().next().unwrap();
                result.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    Err("unterminated string".to_string())
}

fn parse_object(input: &str, pos: usize) -> Result<(JsonValue, usize), String> {
    // pos is at '{'
    let mut i = skip_ws(input, pos + 1);
    let mut pairs = Vec::new();

    if i < input.len() && input.as_bytes()[i] == b'}' {
        return Ok((JsonValue::Object(pairs), i + 1));
    }

    loop {
        let i2 = skip_ws(input, i);
        if i2 >= input.len() {
            return Err("unterminated object".to_string());
        }
        let (key, i3) = parse_string(input, i2)?;
        let i4 = skip_ws(input, i3);
        if i4 >= input.len() || input.as_bytes()[i4] != b':' {
            return Err(format!("expected ':' at {i4}"));
        }
        let (val, i5) = parse_value(input, i4 + 1)?;
        pairs.push((key, val));
        let i6 = skip_ws(input, i5);
        if i6 >= input.len() {
            return Err("unterminated object".to_string());
        }
        if input.as_bytes()[i6] == b'}' {
            return Ok((JsonValue::Object(pairs), i6 + 1));
        }
        if input.as_bytes()[i6] != b',' {
            return Err(format!("expected ',' or '}}' at {i6}"));
        }
        i = i6 + 1;
    }
}

fn parse_array(input: &str, pos: usize) -> Result<(JsonValue, usize), String> {
    // pos is at '['
    let mut i = skip_ws(input, pos + 1);
    let mut items = Vec::new();

    if i < input.len() && input.as_bytes()[i] == b']' {
        return Ok((JsonValue::Array(items), i + 1));
    }

    loop {
        let (val, i2) = parse_value(input, i)?;
        items.push(val);
        let i3 = skip_ws(input, i2);
        if i3 >= input.len() {
            return Err("unterminated array".to_string());
        }
        if input.as_bytes()[i3] == b']' {
            return Ok((JsonValue::Array(items), i3 + 1));
        }
        if input.as_bytes()[i3] != b',' {
            return Err(format!("expected ',' or ']' at {i3}"));
        }
        i = skip_ws(input, i3 + 1);
    }
}

fn parse_bool(input: &str, pos: usize) -> Result<(JsonValue, usize), String> {
    if input[pos..].starts_with("true") {
        Ok((JsonValue::Bool(true), pos + 4))
    } else if input[pos..].starts_with("false") {
        Ok((JsonValue::Bool(false), pos + 5))
    } else {
        Err(format!("expected bool at {pos}"))
    }
}

fn parse_null(input: &str, pos: usize) -> Result<(JsonValue, usize), String> {
    if input[pos..].starts_with("null") {
        Ok((JsonValue::Null, pos + 4))
    } else {
        Err(format!("expected null at {pos}"))
    }
}

fn parse_number(input: &str, pos: usize) -> Result<(JsonValue, usize), String> {
    let bytes = input.as_bytes();
    let mut i = pos;
    if i < bytes.len() && bytes[i] == b'-' {
        i += 1;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        i += 1;
        if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
            i += 1;
        }
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    let s = &input[pos..i];
    let n: f64 = s.parse().map_err(|_| format!("invalid number: {s}"))?;
    Ok((JsonValue::Num(n), i))
}
