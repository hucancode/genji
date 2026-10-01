//! File-backed store for formal-mode tickets.
//!
//! All tickets live as Markdown files directly under `.genji/tickets/`:
//!
//! ```text
//! .genji/tickets/
//!   7-add-rate-limit.md
//!   8-write-tests.md
//! ```
//!
//! Metadata lives in simple `key: value` frontmatter (`id`, `status`,
//! `priority`, `parent`, `requirement`, `mode`, `created`, `updated`); the text
//! after the first `# Heading` is the ticket description. Status and
//! resolution remain in the Markdown frontmatter; there is no database archive
//! or migration path.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::db::TicketEdit;
use crate::util::slugify;

#[derive(Debug, Clone)]
pub struct Ticket {
    pub id: i64,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: i64,
    pub parent_id: Option<i64>,
    pub requirement_id: Option<i64>,
    pub mode: Option<String>,
    pub resolution: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Backing markdown file. `None` for tickets read out of the database
    /// (resolved/closed ones), which are not backed by a file.
    pub path: Option<PathBuf>,
}

impl Ticket {
    /// Display path relative to the workspace (falls back to the absolute path
    /// when it is outside).
    pub fn display_path(&self, workspace: &Path) -> Option<String> {
        self.path.as_ref().map(|p| rel_to(workspace, p))
    }
}

fn now_secs() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

/// Split leading `---` frontmatter from the markdown body.
fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let mut meta = BTreeMap::new();
    if let Some(rest) = text.strip_prefix("---\n")
        && let Some(idx) = rest.find("\n---")
    {
        let fm = &rest[..idx];
        let after = &rest[idx + 4..];
        for line in fm.lines() {
            if let Some((k, v)) = line.split_once(':') {
                meta.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
            }
        }
        let body = after.strip_prefix('\n').unwrap_or(after);
        return (meta, body.to_string());
    }
    (meta, text.to_string())
}

/// Remove the first level-1 heading and return it separately.
fn split_heading(text: &str) -> (Option<String>, String) {
    let mut heading = None;
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if heading.is_none()
            && let Some(h) = line.strip_prefix("# ")
        {
            heading = Some(h.trim().to_string());
            continue;
        }
        lines.push(line);
    }
    (heading, lines.join("\n").trim().to_string())
}

fn render(t: &Ticket) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("id: {}\n", t.id));
    s.push_str(&format!("status: {}\n", t.status));
    s.push_str(&format!("priority: {}\n", t.priority));
    if let Some(p) = t.parent_id {
        s.push_str(&format!("parent: {p}\n"));
    }
    if let Some(r) = t.requirement_id {
        s.push_str(&format!("requirement: {r}\n"));
    }
    if let Some(m) = &t.mode
        && !m.trim().is_empty()
    {
        s.push_str(&format!("mode: {m}\n"));
    }
    if let Some(r) = &t.resolution {
        s.push_str(&format!("resolution: {r}\n"));
    }
    s.push_str(&format!("created: {}\n", t.created_at));
    s.push_str(&format!("updated: {}\n", t.updated_at));
    s.push_str("---\n\n");
    s.push_str(&format!("# {}\n", t.title));
    if !t.description.trim().is_empty() {
        s.push('\n');
        s.push_str(t.description.trim());
        s.push('\n');
    }
    s
}

fn write_at(path: &Path, t: &Ticket) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, render(t)).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
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

fn rel_to(workspace: &Path, path: &Path) -> String {
    path.strip_prefix(workspace)
        .unwrap_or(path)
        .to_string_lossy()
        .to_string()
}

/// Where a ticket's file lives: `<root>/<id>-<slug>.md`.
pub fn path_for(cfg: &Config, workspace: &Path, t: &Ticket) -> PathBuf {
    cfg.tickets_path(workspace)
        .join(format!("{}-{}.md", t.id, slugify(&t.title, "ticket")))
}

/// Load every open ticket file. Files missing an `id` are assigned one and
/// rewritten so identity is stable across runs.
pub fn load_all(cfg: &Config, workspace: &Path) -> Result<Vec<Ticket>> {
    let root = cfg.tickets_path(workspace);
    let mut files = Vec::new();
    walk(&root, &mut files)?;
    files.sort();

    struct Raw {
        meta: BTreeMap<String, String>,
        heading: Option<String>,
        body: String,
        path: PathBuf,
    }

    let mut raws = Vec::new();
    let mut max_id = 0i64;
    for path in files {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        if text.trim().is_empty() {
            continue;
        }
        let (meta, body_raw) = split_frontmatter(&text);
        let (heading, body) = split_heading(&body_raw);
        if let Some(id) = meta.get("id").and_then(|v| v.parse::<i64>().ok()) {
            max_id = max_id.max(id);
        }
        raws.push(Raw {
            meta,
            heading,
            body,
            path,
        });
    }

    let mut next_id = max_id + 1;
    let mut out = Vec::new();
    for raw in raws {
        let had_id = raw
            .meta
            .get("id")
            .and_then(|v| v.parse::<i64>().ok())
            .is_some();
        let id = raw
            .meta
            .get("id")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or_else(|| {
                let id = next_id;
                next_id += 1;
                id
            });
        let title = raw
            .meta
            .get("title")
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .or(raw.heading)
            .unwrap_or_else(|| {
                raw.path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("ticket")
                    .to_string()
            });
        let status = raw
            .meta
            .get("status")
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "open".into());
        let priority = raw
            .meta
            .get("priority")
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(2)
            .clamp(1, 3);
        let parent_id = raw.meta.get("parent").and_then(|v| v.parse::<i64>().ok());
        let requirement_id = raw
            .meta
            .get("requirement")
            .and_then(|v| v.parse::<i64>().ok());
        let mode = raw
            .meta
            .get("mode")
            .filter(|s| !s.trim().is_empty())
            .cloned();
        let resolution = raw
            .meta
            .get("resolution")
            .filter(|s| !s.trim().is_empty())
            .cloned();
        let created_at = raw
            .meta
            .get("created")
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(now_secs);
        let updated_at = raw
            .meta
            .get("updated")
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| created_at.clone());

        let ticket = Ticket {
            id,
            title,
            description: raw.body,
            status,
            priority,
            parent_id,
            requirement_id,
            mode,
            resolution,
            created_at,
            updated_at,
            path: Some(raw.path.clone()),
        };
        // Persist a newly assigned id (or fill in missing metadata) once.
        if !had_id {
            write_at(&raw.path, &ticket)?;
        }
        out.push(ticket);
    }

    out.sort_by_key(|t| (t.priority, t.id));
    Ok(out)
}

pub fn load_by_id(cfg: &Config, workspace: &Path, id: i64) -> Result<Option<Ticket>> {
    Ok(load_all(cfg, workspace)?.into_iter().find(|t| t.id == id))
}

/// Next ticket id from the Markdown store.
pub fn next_id(cfg: &Config, workspace: &Path) -> Result<i64> {
    Ok(load_all(cfg, workspace)?
        .iter()
        .map(|t| t.id)
        .max()
        .unwrap_or(0)
        + 1)
}

#[allow(clippy::too_many_arguments)]
pub fn create(
    cfg: &Config,
    workspace: &Path,
    title: &str,
    description: &str,
    priority: i64,
    parent_id: Option<i64>,
    requirement_id: Option<i64>,
    mode: &str,
) -> Result<Ticket> {
    let now = now_secs();
    let mut ticket = Ticket {
        id: next_id(cfg, workspace)?,
        title: title.to_string(),
        description: description.to_string(),
        status: "open".into(),
        priority: priority.clamp(1, 3),
        parent_id,
        requirement_id,
        mode: if mode.trim().is_empty() {
            None
        } else {
            Some(mode.to_string())
        },
        resolution: None,
        created_at: now.clone(),
        updated_at: now,
        path: None,
    };
    let path = path_for(cfg, workspace, &ticket);
    ticket.path = Some(path.clone());
    write_at(&path, &ticket)?;
    Ok(ticket)
}

/// Apply a field edit and/or status/resolution change to a file-backed ticket.
/// Returns the updated ticket, or `None` when the id has no file.
pub fn update(
    cfg: &Config,
    workspace: &Path,
    id: i64,
    edit: &TicketEdit,
    status: Option<&str>,
    resolution: Option<&str>,
) -> Result<Option<Ticket>> {
    let mut ticket = match load_by_id(cfg, workspace, id)? {
        Some(t) => t,
        None => return Ok(None),
    };
    if let Some(v) = &edit.title {
        ticket.title = v.clone();
    }
    if let Some(v) = &edit.description {
        ticket.description = v.clone();
    }
    if let Some(v) = edit.priority {
        ticket.priority = v.clamp(1, 3);
    }
    if let Some(v) = edit.parent_id {
        ticket.parent_id = v;
    }
    if let Some(v) = edit.requirement_id {
        ticket.requirement_id = v;
    }
    if let Some(s) = status {
        ticket.status = s.to_string();
    }
    if let Some(r) = resolution {
        ticket.resolution = Some(r.to_string());
    }
    ticket.updated_at = now_secs();

    let old_path = ticket.path.clone();
    let new_path = path_for(cfg, workspace, &ticket);
    ticket.path = Some(new_path.clone());
    write_at(&new_path, &ticket)?;
    if let Some(old) = old_path
        && old != new_path
        && old.exists()
    {
        let _ = std::fs::remove_file(&old);
    }
    Ok(Some(ticket))
}

/// Load and normalize Markdown tickets. Markdown is the only source of truth.
pub fn sync(cfg: &Config, workspace: &Path) -> Result<usize> {
    Ok(load_all(cfg, workspace)?.len())
}
