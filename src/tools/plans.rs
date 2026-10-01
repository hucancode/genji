use anyhow::{Context, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::{opt_str, req_str};
use crate::agent::Agent;
use crate::config::{Config, expand_tilde};
use crate::util::slugify;

/// Persist a plan and return the path it was written to. Split out from the
/// tool handler so it can be tested without constructing a full [`Agent`].
///
/// The title drives the default file name (`plans_dir/<slug>.md`) and, when the
/// content has no `# Heading` of its own, the heading. Plans live under
/// `plans_dir` (default `.genji/plans/`) so the plan outlives the run and can be
/// reviewed or versioned with the code. Passing the same title again refines an
/// existing plan in place; `path` overrides the location entirely.
pub fn write_plan(
    cfg: &Config,
    workspace: &Path,
    title: &str,
    content: &str,
    path: Option<&str>,
) -> Result<PathBuf> {
    let rel = match path {
        Some(p) => p.to_string(),
        None => format!(
            "{}/{}.md",
            cfg.plans_dir.trim_end_matches('/'),
            slugify(title, "plan")
        ),
    };
    let expanded = expand_tilde(&rel);
    let path = if expanded.is_absolute() {
        expanded
    } else {
        workspace.join(expanded)
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Guarantee a top-level heading so the file reads as a plan even when the
    // model did not include one.
    let body = if content.trim_start().starts_with("# ") {
        content.to_string()
    } else {
        format!("# {}\n\n{}", title.trim(), content.trim_start())
    };
    std::fs::write(&path, &body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// `plan_write` — persist an implementation plan as a markdown file.
pub fn write(agent: &mut Agent, args: &Value) -> Result<String> {
    let title = req_str(args, "title")?;
    let content = req_str(args, "content")?;
    let explicit = opt_str(args, "path");
    let path = write_plan(
        &agent.cfg,
        &agent.workspace,
        &title,
        &content,
        explicit.as_deref(),
    )?;
    Ok(format!("wrote plan to {}", agent.display_path(&path)))
}

#[cfg(test)]
mod tests {
    use super::write_plan;
    use crate::config::Config;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("genji-plans-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writes_plan_under_plans_dir_with_heading() {
        let ws = temp_workspace("write");
        let cfg = Config::default();
        let path = write_plan(&cfg, &ws, "Rate limiting", "Step one.", None).unwrap();
        assert_eq!(
            path.strip_prefix(&ws).unwrap().to_string_lossy(),
            ".genji/plans/rate-limiting.md"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text, "# Rate limiting\n\nStep one.");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn keeps_existing_heading_and_honours_explicit_path() {
        let ws = temp_workspace("path");
        let cfg = Config::default();
        let path = write_plan(
            &cfg,
            &ws,
            "Ignored",
            "# Custom\n\nBody.",
            Some("notes/my-plan.md"),
        )
        .unwrap();
        assert!(path.ends_with("notes/my-plan.md"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Custom\n\nBody.");
        let _ = std::fs::remove_dir_all(&ws);
    }
}
