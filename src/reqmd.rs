use anyhow::{Context, Result};
use std::path::Path;

use crate::config::Config;
use crate::db::Db;

/// Parse `level:` from simple frontmatter, if present.
fn frontmatter_level(text: &str) -> Option<String> {
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(idx) = rest.find("\n---") {
            for line in rest[..idx].lines() {
                if let Some(v) = line.strip_prefix("level:") {
                    let v = v.trim().trim_matches('"').to_string();
                    if v == "stakeholder" || v == "system" {
                        return Some(v);
                    }
                }
            }
        }
    }
    None
}

fn infer_level(rel_path: &str, text: &str) -> String {
    if let Some(l) = frontmatter_level(text) {
        return l;
    }
    let lower = rel_path.to_ascii_lowercase();
    if lower.contains("system") {
        "system".into()
    } else {
        "stakeholder".into()
    }
}

fn first_heading(text: &str) -> Option<String> {
    for line in text.lines() {
        let t = line.trim();
        if let Some(h) = t.strip_prefix("# ") {
            return Some(h.trim().to_string());
        }
    }
    None
}

fn walk(dir: &Path, out: &mut Vec<std::path::PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            out.push(path);
        }
    }
    Ok(())
}

/// Ingest user-authored markdown requirement files into the database.
/// Each file becomes one requirement, keyed by its path so edits update it.
pub fn sync_requirements_md(db: &Db, cfg: &Config, workspace: &Path) -> Result<(usize, usize)> {
    let dir = cfg.requirements_path(workspace);
    let mut files = Vec::new();
    walk(&dir, &mut files)?;
    let mut total = 0;
    let mut changed = 0;
    for path in files {
        let text = std::fs::read_to_string(&path)?;
        if text.trim().is_empty() {
            continue;
        }
        let rel = path
            .strip_prefix(workspace)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        let level = infer_level(&rel, &text);
        let title = first_heading(&text).unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("requirement")
                .to_string()
        });
        let (_, did_change) = db.requirement_upsert_source(&level, &title, &text, &rel)?;
        total += 1;
        if did_change {
            changed += 1;
        }
    }
    Ok((total, changed))
}

#[cfg(test)]
mod tests {
    use super::{first_heading, infer_level};

    #[test]
    fn level_from_directory() {
        assert_eq!(infer_level("requirements/system/a.md", "# x"), "system");
        assert_eq!(infer_level("requirements/stakeholder/a.md", "# x"), "stakeholder");
    }

    #[test]
    fn level_from_frontmatter_wins() {
        let text = "---\nlevel: system\n---\n# x\n";
        assert_eq!(infer_level("requirements/stakeholder/a.md", text), "system");
    }

    #[test]
    fn heading_extraction() {
        assert_eq!(first_heading("intro\n# Real Title\nbody"), Some("Real Title".into()));
        assert_eq!(first_heading("no heading"), None);
    }
}
