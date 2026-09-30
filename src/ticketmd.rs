//! File-backed store for *open* tickets.
//!
//! Tickets that are still being worked (`open` or `in_progress`) live as
//! markdown files directly under `.genji/tickets/`:
//!
//! ```text
//! .genji/tickets/
//!   7-add-rate-limit.md
//!   8-write-tests.md
//! ```
//!
//! Metadata lives in simple `key: value` frontmatter (`id`, `status`,
//! `priority`, `parent`, `requirement`, `mode`, `created`, `updated`); the text
//! after the first `# Heading` is the ticket description. Once a ticket is
//! resolved or closed it leaves this store and is archived in the SQLite
//! `tickets` table (see [`crate::db::Db::ticket_insert`]). `move_to_db` /
//! `move_to_file` in `tools::tickets` drive that transition.

use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Config;
use crate::db::{Db, TicketEdit};
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
        self.path
            .as_ref()
            .map(|p| rel_to(workspace, p))
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
    if let Some(m) = &t.mode {
        if !m.trim().is_empty() {
            s.push_str(&format!("mode: {m}\n"));
        }
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
    Ok(load_all(cfg, workspace)?
        .into_iter()
        .find(|t| t.id == id))
}

/// Next ticket id, taken across both stores so ids stay unique and stable.
pub fn next_id(db: &Db, cfg: &Config, workspace: &Path) -> Result<i64> {
    let file_max = load_all(cfg, workspace)?
        .iter()
        .map(|t| t.id)
        .max()
        .unwrap_or(0);
    Ok(file_max.max(db.ticket_max_id()?) + 1)
}

#[allow(clippy::too_many_arguments)]
pub fn create(
    db: &Db,
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
        id: next_id(db, cfg, workspace)?,
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
    if let Some(old) = old_path {
        if old != new_path && old.exists() {
            let _ = std::fs::remove_file(&old);
        }
    }
    Ok(Some(ticket))
}

/// Write a ticket to its file (used when a DB ticket is reopened).
pub fn save(cfg: &Config, workspace: &Path, ticket: &Ticket) -> Result<()> {
    let path = path_for(cfg, workspace, ticket);
    write_at(&path, ticket)
}

/// One-time migration of legacy open/in_progress DB tickets into files. Only
/// rows that are still actionable are moved; resolved/closed history stays in
/// the database.
pub fn sync(db: &Db, cfg: &Config, workspace: &Path) -> Result<usize> {
    let open = db.ticket_list(None, None)?;
    let existing: std::collections::BTreeSet<i64> = load_all(cfg, workspace)?
        .iter()
        .map(|t| t.id)
        .collect();
    let mut migrated = 0;
    for t in open {
        if !matches!(t.status.as_str(), "open" | "in_progress") {
            continue;
        }
        if existing.contains(&t.id) {
            continue;
        }
        let ticket = Ticket {
            id: t.id,
            title: t.title,
            description: t.description,
            status: t.status,
            priority: t.priority,
            parent_id: t.parent_id,
            requirement_id: t.requirement_id,
            mode: t.mode,
            resolution: t.resolution,
            created_at: t.created_at,
            updated_at: t.updated_at,
            path: None,
        };
        save(cfg, workspace, &ticket)?;
        db.ticket_delete(t.id)?;
        migrated += 1;
    }
    if migrated > 0 {
        eprintln!(
            "[tickets] migrated {migrated} open ticket(s) from the database to {}",
            cfg.tickets_path(workspace).display()
        );
    }
    Ok(load_all(cfg, workspace)?.len())
}

#[cfg(test)]
mod tests {
    use super::{create, load_all, load_by_id, next_id, save, update};
    use crate::config::Config;
    use crate::db::{Db, TicketEdit};

    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("genji-ticketmd-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn memory_db() -> Db {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let db = Db { conn };
        db.init_schema().unwrap();
        db
    }

    #[test]
    fn file_store_lifecycle() {
        let ws = temp_workspace("lifecycle");
        let cfg = Config::default();
        let db = memory_db();

        let t = create(
            &db,
            &cfg,
            &ws,
            "Add rate limit",
            "Reject more than 100 req/min.",
            1,
            None,
            Some(3),
            "plan",
        )
        .unwrap();
        assert_eq!(t.id, 1);
        assert!(t.path.as_ref().unwrap().exists());
        assert!(t
            .display_path(&ws)
            .unwrap()
            .ends_with(".genji/tickets/1-add-rate-limit.md"));

        let edit = TicketEdit {
            title: Some("Add API rate limit".into()),
            priority: Some(3),
            ..Default::default()
        };
        let updated = update(&cfg, &ws, 1, &edit, Some("in_progress"), None)
            .unwrap()
            .unwrap();
        assert_eq!(updated.title, "Add API rate limit");
        assert_eq!(updated.status, "in_progress");
        assert_eq!(updated.priority, 3);
        // Renaming the title renames the file and removes the old one.
        assert!(updated
            .display_path(&ws)
            .unwrap()
            .ends_with("1-add-api-rate-limit.md"));
        assert!(!ws
            .join(".genji/tickets/1-add-rate-limit.md")
            .exists());

        assert_eq!(load_all(&cfg, &ws).unwrap().len(), 1);
        assert!(load_by_id(&cfg, &ws, 1).unwrap().is_some());
        assert_eq!(next_id(&db, &cfg, &ws).unwrap(), 2);
    }

    #[test]
    fn db_id_steers_next_file_id() {
        let ws = temp_workspace("db-id");
        let cfg = Config::default();
        let db = memory_db();
        db.ticket_insert(
            9,
            "archived",
            "",
            "resolved",
            2,
            None,
            None,
            None,
            Some("done"),
            None,
        )
        .unwrap();
        assert_eq!(create(&db, &cfg, &ws, "next", "", 2, None, None, "build").unwrap().id, 10);
    }

    #[test]
    fn reopen_round_trip_through_db() {
        let ws = temp_workspace("reopen");
        let cfg = Config::default();
        let db = memory_db();
        let mut t = create(&db, &cfg, &ws, "Fix bug", "broken", 2, None, None, "build").unwrap();
        t.status = "resolved".into();
        t.resolution = Some("fixed".into());
        // archive into the DB and drop the file
        db.ticket_insert(
            t.id,
            &t.title,
            &t.description,
            &t.status,
            t.priority,
            t.parent_id,
            t.requirement_id,
            t.mode.as_deref(),
            t.resolution.as_deref(),
            Some(&t.created_at),
        )
        .unwrap();
        std::fs::remove_file(t.path.as_ref().unwrap()).unwrap();

        assert!(db.ticket_get(t.id).unwrap().is_some());
        assert!(load_by_id(&cfg, &ws, t.id).unwrap().is_none());

        // reopen back into a file
        let mut back = t.clone();
        back.status = "open".into();
        back.resolution = None;
        save(&cfg, &ws, &back).unwrap();
        db.ticket_delete(t.id).unwrap();

        let reopened = load_by_id(&cfg, &ws, t.id).unwrap().unwrap();
        assert_eq!(reopened.status, "open");
        assert!(reopened.resolution.is_none());
        assert!(db.ticket_get(t.id).unwrap().is_none());
    }

    #[test]
    fn sync_migrates_open_rows_only() {
        let ws = temp_workspace("sync");
        let cfg = Config::default();
        let db = memory_db();
        db.ticket_insert(1, "open one", "", "open", 2, None, Some(3), Some("plan"), None, None)
            .unwrap();
        db.ticket_insert(2, "working", "", "in_progress", 1, None, None, None, None, None)
            .unwrap();
        db.ticket_insert(
            3,
            "archived",
            "",
            "resolved",
            2,
            None,
            None,
            None,
            Some("done"),
            None,
        )
        .unwrap();

        let files = super::sync(&db, &cfg, &ws).unwrap();
        assert_eq!(files, 2);
        // Open rows moved to files and out of the DB; resolved history stayed.
        assert!(load_by_id(&cfg, &ws, 1).unwrap().is_some());
        assert!(load_by_id(&cfg, &ws, 2).unwrap().is_some());
        assert!(db.ticket_get(1).unwrap().is_none());
        assert!(db.ticket_get(2).unwrap().is_none());
        assert!(db.ticket_get(3).unwrap().is_some());

        // Idempotent: a second pass finds nothing left to move.
        assert_eq!(super::sync(&db, &cfg, &ws).unwrap(), 2);
    }

    #[test]
    fn frontmatter_and_heading_round_trip() {
        let ws = temp_workspace("round");
        let cfg = Config::default();
        let dir = cfg.tickets_path(&ws);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("hand-written.md"),
            "---\nstatus: in_progress\npriority: 1\nrequirement: 4\n---\n# Hand Written\nBody.\n",
        )
        .unwrap();

        let all = load_all(&cfg, &ws).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, 1);
        assert_eq!(all[0].title, "Hand Written");
        assert_eq!(all[0].status, "in_progress");
        assert_eq!(all[0].priority, 1);
        assert_eq!(all[0].requirement_id, Some(4));
        // The id is persisted back into the file.
        let text = std::fs::read_to_string(dir.join("hand-written.md")).unwrap();
        assert!(text.contains("id: 1"), "frontmatter should gain an id:\n{text}");
    }
}
