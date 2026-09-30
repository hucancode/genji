//! File-backed requirement store.
//!
//! Requirements are markdown files under `.genji/requirements/`:
//!
//! ```text
//! .genji/requirements/
//!   stakeholder/1-cat-image-classifier.md
//!   system/2-accept-image-files-and-urls.md
//! ```
//!
//! Metadata lives in simple `key: value` frontmatter (`id`, `level`, `status`,
//! `parent`, `source`, `created`, `updated`); the text after the first
//! `# Heading` is the requirement body

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::db::Db;
use crate::util::slugify;

#[derive(Debug, Clone)]
pub struct Requirement {
    pub id: i64,
    pub level: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub parent_id: Option<i64>,
    pub source: String,
    /// Absolute path of the backing markdown file. The single source of truth
    /// for where the requirement lives; the display form is [`Self::display_path`].
    pub path: PathBuf,
    pub created_at: String,
    pub updated_at: String,
}

impl Requirement {
    /// Display path relative to the workspace (falls back to the absolute path
    /// when it is outside).
    pub fn display_path(&self, workspace: &Path) -> String {
        rel_to(workspace, &self.path)
    }
}

fn now_secs() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
        .to_string()
}

fn valid_level(v: &str) -> Option<String> {
    let v = v.trim().trim_matches('"').to_ascii_lowercase();
    if v == "stakeholder" || v == "system" {
        Some(v)
    } else {
        None
    }
}

fn infer_level(rel_path: &str) -> String {
    if rel_path.to_ascii_lowercase().contains("system") {
        "system".into()
    } else {
        "stakeholder".into()
    }
}

/// Split leading `---` frontmatter from the markdown body.
fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let mut meta = BTreeMap::new();
    if let Some(rest) = text.strip_prefix("---\n") {
        if let Some(idx) = rest.find("\n---") {
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
    }
    (meta, text.to_string())
}

/// Remove the first level-1 heading and return it separately.
fn split_heading(text: &str) -> (Option<String>, String) {
    let mut heading = None;
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if heading.is_none() {
            if let Some(h) = line.strip_prefix("# ") {
                heading = Some(h.trim().to_string());
                continue;
            }
        }
        lines.push(line);
    }
    (heading, lines.join("\n").trim().to_string())
}

fn render(r: &Requirement) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("id: {}\n", r.id));
    s.push_str(&format!("level: {}\n", r.level));
    s.push_str(&format!("status: {}\n", r.status));
    if let Some(p) = r.parent_id {
        s.push_str(&format!("parent: {p}\n"));
    }
    s.push_str(&format!("source: {}\n", r.source));
    s.push_str(&format!("created: {}\n", r.created_at));
    s.push_str(&format!("updated: {}\n", r.updated_at));
    s.push_str("---\n\n");
    s.push_str(&format!("# {}\n", r.title));
    if !r.body.trim().is_empty() {
        s.push('\n');
        s.push_str(r.body.trim());
        s.push('\n');
    }
    s
}

fn write_at(path: &Path, r: &Requirement) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, render(r)).with_context(|| format!("writing {}", path.display()))?;
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

/// Where a requirement's file lives: `<root>/<level>/<id>-<slug>.md`.
fn path_for(cfg: &Config, workspace: &Path, r: &Requirement) -> PathBuf {
    cfg.requirements_path(workspace)
        .join(&r.level)
        .join(format!("{}-{}.md", r.id, slugify(&r.title, "requirement")))
}

/// Load every requirement file. Files missing an `id` are assigned one and
/// rewritten so identity is stable across runs.
pub fn load_all(cfg: &Config, workspace: &Path) -> Result<Vec<Requirement>> {
    let root = cfg.requirements_path(workspace);
    let mut files = Vec::new();
    walk(&root, &mut files)?;
    files.sort();

    struct Raw {
        meta: BTreeMap<String, String>,
        heading: Option<String>,
        body: String,
        path: PathBuf,
        rel: String,
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
        let rel = rel_to(workspace, &path);
        raws.push(Raw {
            meta,
            heading,
            body,
            path,
            rel,
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
        let level = raw
            .meta
            .get("level")
            .and_then(|v| valid_level(v))
            .unwrap_or_else(|| infer_level(&raw.rel));
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
                    .unwrap_or("requirement")
                    .to_string()
            });
        let status = raw
            .meta
            .get("status")
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "active".into());
        let parent_id = raw.meta.get("parent").and_then(|v| v.parse::<i64>().ok());
        let source = raw
            .meta
            .get("source")
            .filter(|s| !s.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| "user_md".into());
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

        let req = Requirement {
            id,
            level,
            title,
            body: raw.body,
            status,
            parent_id,
            source,
            path: raw.path.clone(),
            created_at,
            updated_at,
        };
        // Persist a newly assigned id (or fill in missing metadata) once.
        if !had_id {
            write_at(&raw.path, &req)?;
        }
        out.push(req);
    }

    out.sort_by(|a, b| {
        let rank = |r: &Requirement| if r.level == "stakeholder" { 0 } else { 1 };
        rank(a).cmp(&rank(b)).then(a.id.cmp(&b.id))
    });
    Ok(out)
}

pub fn load_by_id(cfg: &Config, workspace: &Path, id: i64) -> Result<Option<Requirement>> {
    Ok(load_all(cfg, workspace)?.into_iter().find(|r| r.id == id))
}

pub fn active_count(cfg: &Config, workspace: &Path) -> Result<i64> {
    Ok(load_all(cfg, workspace)?
        .iter()
        .filter(|r| r.status == "active")
        .count() as i64)
}

pub fn create(
    cfg: &Config,
    workspace: &Path,
    level: &str,
    title: &str,
    body: &str,
    parent_id: Option<i64>,
    source: &str,
) -> Result<Requirement> {
    let Some(level) = valid_level(level) else {
        bail!("level must be `stakeholder` or `system`");
    };
    let id = load_all(cfg, workspace)?
        .iter()
        .map(|r| r.id)
        .max()
        .unwrap_or(0)
        + 1;
    let now = now_secs();
    let mut req = Requirement {
        id,
        level,
        title: title.to_string(),
        body: body.to_string(),
        status: "active".into(),
        parent_id,
        source: source.to_string(),
        path: PathBuf::new(),
        created_at: now.clone(),
        updated_at: now,
    };
    let path = path_for(cfg, workspace, &req);
    req.path = path.clone();
    write_at(&path, &req)?;
    Ok(req)
}

#[allow(clippy::too_many_arguments)]
pub fn update(
    cfg: &Config,
    workspace: &Path,
    id: i64,
    title: Option<&str>,
    body: Option<&str>,
    status: Option<&str>,
    level: Option<&str>,
    parent_id: Option<Option<i64>>,
) -> Result<bool> {
    let mut req = match load_by_id(cfg, workspace, id)? {
        Some(r) => r,
        None => return Ok(false),
    };
    if let Some(l) = level {
        let Some(l) = valid_level(l) else {
            bail!("level must be stakeholder|system");
        };
        req.level = l;
    }
    if let Some(t) = title {
        req.title = t.to_string();
    }
    if let Some(b) = body {
        req.body = b.to_string();
    }
    if let Some(s) = status {
        req.status = s.to_string();
    }
    if let Some(p) = parent_id {
        req.parent_id = p;
    }
    req.updated_at = now_secs();

    let old_path = req.path.clone();
    let new_path = path_for(cfg, workspace, &req);
    req.path = new_path.clone();
    write_at(&new_path, &req)?;
    if new_path != old_path && old_path.exists() {
        let _ = std::fs::remove_file(&old_path);
    }
    Ok(true)
}

pub fn remove(cfg: &Config, workspace: &Path, id: i64, hard: bool) -> Result<bool> {
    let Some(req) = load_by_id(cfg, workspace, id)? else {
        return Ok(false);
    };
    if hard {
        std::fs::remove_file(&req.path)
            .with_context(|| format!("deleting {}", req.path.display()))?;
        return Ok(true);
    }
    update(cfg, workspace, id, None, None, Some("removed"), None, None)
}

/// One-time migration of legacy DB requirements into markdown files. Runs only
/// when no requirement files exist yet, preserving ids so tickets keep working.
fn migrate_from_db(db: &Db, cfg: &Config, workspace: &Path) -> Result<usize> {
    if !db.has_table("requirements")? {
        return Ok(0);
    }
    if !load_all(cfg, workspace)?.is_empty() {
        return Ok(0);
    }
    let reqs = db.requirement_list(None, None)?;
    let mut n = 0;
    for r in reqs {
        let mut req = Requirement {
            id: r.id,
            level: r.level,
            title: r.title,
            body: r.body,
            status: r.status,
            parent_id: r.parent_id,
            source: r.source,
            path: PathBuf::new(),
            created_at: r.created_at,
            updated_at: r.updated_at,
        };
        let path = path_for(cfg, workspace, &req);
        req.path = path.clone();
        write_at(&path, &req)?;
        n += 1;
    }
    Ok(n)
}

/// Load (and normalize) all requirement files, migrating legacy DB rows on the
/// first run. Returns the number of requirement files present.
pub fn sync(db: &Db, cfg: &Config, workspace: &Path) -> Result<usize> {
    let migrated = migrate_from_db(db, cfg, workspace)?;
    if migrated > 0 {
        eprintln!(
            "[requirements] migrated {migrated} requirement(s) from the database to markdown"
        );
    }
    Ok(load_all(cfg, workspace)?.len())
}

#[cfg(test)]
mod tests {
    use super::{
        active_count, create, infer_level, load_all, load_by_id, remove, split_frontmatter,
        split_heading, update,
    };
    use crate::config::Config;
    use crate::util::slugify;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn level_from_directory() {
        assert_eq!(infer_level("requirements/system/a.md"), "system");
        assert_eq!(infer_level("requirements/stakeholder/a.md"), "stakeholder");
    }

    #[test]
    fn heading_extraction() {
        assert_eq!(
            split_heading("intro\n# Real Title\nbody"),
            (Some("Real Title".into()), "intro\nbody".into())
        );
        assert_eq!(split_heading("no heading"), (None, "no heading".into()));
    }

    #[test]
    fn frontmatter_parsing() {
        let (meta, body) = split_frontmatter("---\nid: 4\nlevel: system\n---\n# T\nbody\n");
        assert_eq!(meta.get("id").map(String::as_str), Some("4"));
        assert_eq!(meta.get("level").map(String::as_str), Some("system"));
        assert_eq!(body, "# T\nbody\n");
    }

    #[test]
    fn slugging() {
        assert_eq!(
            slugify("Accept image files & URLs!", "requirement"),
            "accept-image-files-urls"
        );
        assert_eq!(slugify("", "requirement"), "requirement");
    }

    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("genji-reqmd-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_store_lifecycle() {
        let ws = temp_workspace("lifecycle");
        let cfg = Config::default();

        let r1 = create(
            &cfg,
            &ws,
            "stakeholder",
            "Cat Classifier",
            "Must classify cats.",
            None,
            "agent",
        )
        .unwrap();
        assert_eq!(r1.id, 1);
        assert!(r1.path.exists());

        let r2 = create(
            &cfg,
            &ws,
            "system",
            "Accept URLs",
            "Accept image URLs.",
            Some(1),
            "agent",
        )
        .unwrap();
        assert_eq!(r2.id, 2);
        assert_eq!(active_count(&cfg, &ws).unwrap(), 2);

        // Editing the title renames the backing file.
        assert!(update(
            &cfg,
            &ws,
            2,
            Some("Accept Files and URLs"),
            None,
            Some("met"),
            None,
            None
        )
        .unwrap());
        let r2b = load_by_id(&cfg, &ws, 2).unwrap().unwrap();
        assert_eq!(r2b.title, "Accept Files and URLs");
        assert_eq!(r2b.status, "met");
        assert_eq!(r2b.parent_id, Some(1));
        assert!(r2b
            .display_path(&ws)
            .ends_with("2-accept-files-and-urls.md"));
        assert_eq!(active_count(&cfg, &ws).unwrap(), 1);

        // Changing level moves the file into the other directory.
        update(&cfg, &ws, 2, None, None, None, Some("stakeholder"), None).unwrap();
        let r2c = load_by_id(&cfg, &ws, 2).unwrap().unwrap();
        assert_eq!(r2c.level, "stakeholder");
        assert!(r2c
            .display_path(&ws)
            .starts_with(".genji/requirements/stakeholder/"));

        assert!(remove(&cfg, &ws, 2, false).unwrap());
        assert_eq!(active_count(&cfg, &ws).unwrap(), 1);
        assert!(remove(&cfg, &ws, 2, true).unwrap());
        assert!(load_by_id(&cfg, &ws, 2).unwrap().is_none());
        assert_eq!(load_all(&cfg, &ws).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn assigns_missing_id() {
        let ws = temp_workspace("missing-id");
        let cfg = Config::default();
        let dir = cfg.requirements_path(&ws).join("system");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hand-written.md"), "# Hand Written\nBody.\n").unwrap();

        let all = load_all(&cfg, &ws).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, 1);
        assert_eq!(all[0].level, "system");
        // The id is persisted back into the file.
        let text = std::fs::read_to_string(dir.join("hand-written.md")).unwrap();
        assert!(
            text.contains("id: 1"),
            "frontmatter should gain an id:\n{text}"
        );

        let _ = std::fs::remove_dir_all(&ws);
    }
}
