use anyhow::{anyhow, bail, Context, Result};
use serde_json::Value;
use std::time::Duration;

use super::{opt_bool, opt_i64, opt_str, req_str};
use crate::agent::Agent;
use crate::proc;

pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
    let path = agent.resolve_path(&req_str(args, "path")?);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let offset = opt_i64(args, "offset").unwrap_or(1).max(1) as usize;
    let limit = opt_i64(args, "limit").unwrap_or(2000).max(1) as usize;

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    if total == 0 {
        return Ok(format!("{} is empty (0 lines)", path.display()));
    }
    let start = (offset - 1).min(total);
    let end = (start + limit).min(total);
    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>6}\t{}\n", start + i + 1, line));
    }
    if end < total {
        out.push_str(&format!(
            "\n[showing lines {}-{} of {}]\n",
            start + 1,
            end,
            total
        ));
    }
    Ok(out)
}

pub fn write(agent: &mut Agent, args: &Value) -> Result<String> {
    let path = agent.resolve_path(&req_str(args, "path")?);
    let content = req_str(args, "content")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
    Ok(format!(
        "wrote {} bytes to {}",
        content.len(),
        agent.display_path(&path)
    ))
}

fn find_unique(hay: &str, needle: &str) -> Result<usize> {
    if needle.is_empty() {
        bail!("oldText must not be empty");
    }
    let mut it = hay.match_indices(needle);
    match (it.next(), it.next()) {
        (Some((i, _)), None) => Ok(i),
        (None, _) => bail!(
            "oldText not found: {:?}",
            needle.chars().take(60).collect::<String>()
        ),
        (Some(_), Some(_)) => bail!(
            "oldText is not unique: {:?}",
            needle.chars().take(60).collect::<String>()
        ),
    }
}

fn apply_edits(content: &str, edits: &[(String, String)], replace_all: bool) -> Result<String> {
    if edits.len() == 1 && replace_all {
        let (old, new) = &edits[0];
        if old.is_empty() {
            bail!("oldText must not be empty");
        }
        let count = content.matches(old.as_str()).count();
        if count == 0 {
            bail!("oldText not found");
        }
        return Ok(content.replace(old.as_str(), new.as_str()));
    }
    // Locate each edit in the original content, verify uniqueness, then check
    // the ranges do not overlap.
    let mut ranges: Vec<(usize, usize, String)> = Vec::new();
    for (old, new) in edits {
        let start = find_unique(content, old)?;
        ranges.push((start, start + old.len(), new.clone()));
    }
    let mut sorted = ranges;
    sorted.sort_by_key(|r| r.0);
    for w in sorted.windows(2) {
        if w[0].1 > w[1].0 {
            bail!("edit ranges overlap");
        }
    }
    let mut out = content.to_string();
    for (start, end, new) in sorted.into_iter().rev() {
        out.replace_range(start..end, &new);
    }
    Ok(out)
}

pub fn edit(agent: &mut Agent, args: &Value) -> Result<String> {
    let path = agent.resolve_path(&req_str(args, "path")?);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;

    let replace_all = opt_bool(args, "replace_all").unwrap_or(false);
    let mut edits: Vec<(String, String)> = Vec::new();
    if let Some(arr) = args.get("edits").and_then(|v| v.as_array()) {
        for e in arr {
            let old = e
                .get("oldText")
                .or_else(|| e.get("old_text"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("edit entry missing oldText"))?;
            let new = e
                .get("newText")
                .or_else(|| e.get("new_text"))
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("edit entry missing newText"))?;
            edits.push((old.to_string(), new.to_string()));
        }
    } else if let (Some(old), Some(new)) = (opt_str(args, "oldText"), opt_str(args, "newText")) {
        edits.push((old, new));
    }
    if edits.is_empty() {
        bail!("no edits supplied (provide `edits` array or `oldText`/`newText`)");
    }

    let new_content = apply_edits(&content, &edits, replace_all)?;
    std::fs::write(&path, &new_content).with_context(|| format!("writing {}", path.display()))?;
    Ok(format!(
        "applied {} edit(s) to {} ({} -> {} bytes)",
        edits.len(),
        agent.display_path(&path),
        content.len(),
        new_content.len()
    ))
}

pub fn ls(agent: &mut Agent, args: &Value) -> Result<String> {
    let rel = opt_str(args, "path").unwrap_or_else(|| ".".into());
    let root = agent.resolve_path(&rel);
    if !root.exists() {
        bail!("path does not exist: {}", root.display());
    }
    let recursive = opt_bool(args, "recursive").unwrap_or(false);
    let show_hidden = opt_bool(args, "show_hidden").unwrap_or(false);
    let max_depth = opt_i64(args, "max_depth").unwrap_or(if recursive { 6 } else { 1 });

    let mut builder = ignore::WalkBuilder::new(&root);
    builder
        .hidden(!show_hidden)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .ignore(true)
        .parents(false)
        .require_git(false)
        .max_depth(Some(max_depth.max(1) as usize));

    let mut lines: Vec<String> = Vec::new();
    let mut dirs = 0usize;
    let mut files = 0usize;
    for entry in builder.build() {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.depth() == 0 {
            continue; // skip the root itself
        }
        let path = entry.path();
        let rel_path = path
            .strip_prefix(&agent.workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if meta.is_dir() {
            dirs += 1;
            lines.push(format!("d        {rel_path}/"));
        } else {
            files += 1;
            lines.push(format!("f {:>8} {rel_path}", meta.len()));
        }
    }
    lines.sort();
    let mut out = lines.join("\n");
    if out.is_empty() {
        out = "(empty)".into();
    }
    out.push_str(&format!("\n\n[{dirs} dirs, {files} files under {rel}]"));
    Ok(out)
}

pub fn bash(agent: &mut Agent, args: &Value) -> Result<String> {
    let command = req_str(args, "command")?;
    let cwd = match opt_str(args, "cwd") {
        Some(c) => agent.resolve_path(&c),
        None => agent.workspace.clone(),
    };
    let timeout = Duration::from_secs(
        opt_i64(args, "timeout_secs")
            .map(|v| v.max(1) as u64)
            .unwrap_or(agent.cfg.bash_timeout_secs),
    );
    let cap = agent.cfg.tool_result_max_bytes.saturating_mul(2).max(8192);
    let res = proc::run_bash(&command, &cwd, timeout, cap)
        .with_context(|| format!("running command: {command}"))?;
    let mut out = String::new();
    out.push_str(&format!(
        "exit_code: {}\n",
        res.code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "none".into())
    ));
    if res.timed_out {
        out.push_str(&format!("[timed out after {}s]\n", timeout.as_secs()));
    }
    if !res.stdout.is_empty() {
        out.push_str("--- stdout ---\n");
        out.push_str(&res.stdout);
        if !res.stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if !res.stderr.is_empty() {
        out.push_str("--- stderr ---\n");
        out.push_str(&res.stderr);
        if !res.stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::apply_edits;

    #[test]
    fn single_edit() {
        let out = apply_edits("hello world", &[("world".into(), "there".into())], false).unwrap();
        assert_eq!(out, "hello there");
    }

    #[test]
    fn multi_edit_applied_backwards() {
        let edits = vec![
            ("abc".to_string(), "x".to_string()),
            ("ghi".to_string(), "y".to_string()),
        ];
        assert_eq!(apply_edits("abcdefghi", &edits, false).unwrap(), "xdefy");
    }

    #[test]
    fn non_unique_is_error() {
        assert!(apply_edits("aa", &[("a".to_string(), "b".to_string())], false).is_err());
    }

    #[test]
    fn replace_all() {
        assert_eq!(
            apply_edits("aa", &[("a".to_string(), "b".to_string())], true).unwrap(),
            "bb"
        );
    }

    #[test]
    fn overlap_is_error() {
        let edits = vec![
            ("abc".to_string(), "x".to_string()),
            ("cde".to_string(), "y".to_string()),
        ];
        assert!(apply_edits("abcdef", &edits, false).is_err());
    }

    #[test]
    fn missing_is_error() {
        assert!(apply_edits("abc", &[("zzz".to_string(), "x".to_string())], false).is_err());
    }
}
