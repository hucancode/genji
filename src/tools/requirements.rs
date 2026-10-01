use anyhow::{Result, bail};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{opt_i64, opt_str, req_str};
use crate::agent::Agent;
use crate::config::Config;
use crate::reqmd::{self, Requirement};

fn fmt_req(r: &Requirement, workspace: &Path, full: bool) -> String {
    let mut s = format!("#{} [{}:{}] {}\n", r.id, r.level, r.status, r.title);
    if let Some(p) = r.parent_id {
        s.push_str(&format!("parent: #{p}\n"));
    }
    s.push_str(&format!("source: {}\n", r.source));
    s.push_str(&format!("source_path: {}\n", r.display_path(workspace)));
    let body = r.body.trim();
    if !body.is_empty() {
        s.push('\n');
        if full {
            s.push_str(body);
        } else {
            let truncated: String = body.chars().take(200).collect();
            s.push_str(&truncated);
            if body.chars().count() > 200 {
                s.push('…');
            }
        }
        s.push('\n');
    }
    s
}

pub fn create(agent: &mut Agent, args: &Value) -> Result<String> {
    let level = req_str(args, "level")?;
    if level != "stakeholder" && level != "system" {
        bail!("level must be `stakeholder` or `system`");
    }
    let title = req_str(args, "title")?;
    let body = req_str(args, "body")?;
    let parent_id = opt_i64(args, "parent_id");
    let r = reqmd::create(
        &agent.cfg,
        &agent.workspace,
        &level,
        &title,
        &body,
        parent_id,
        "agent",
    )?;
    Ok(format!(
        "created {level} requirement #{}: {} ({})",
        r.id,
        r.title,
        r.display_path(&agent.workspace)
    ))
}

/// Render a `requirement_read`: an exact requirement when `id` is set,
/// otherwise the requirements filtered by `level` and/or `status`.
fn read_requirements(
    cfg: &Config,
    workspace: &Path,
    id: Option<i64>,
    level: Option<&str>,
    status: Option<&str>,
) -> Result<String> {
    if let Some(id) = id {
        return match reqmd::load_by_id(cfg, workspace, id)? {
            Some(r) => Ok(fmt_req(&r, workspace, true)),
            None => bail!("requirement #{id} not found"),
        };
    }
    let reqs: Vec<Requirement> = reqmd::load_all(cfg, workspace)?
        .into_iter()
        .filter(|r| level.is_none_or(|l| r.level == l))
        .filter(|r| status.is_none_or(|s| r.status == s))
        .collect();
    if reqs.is_empty() {
        return Ok("(no requirements)".into());
    }
    // A single hit is shown in full; a list is truncated per requirement so it
    // stays scannable.
    let full = reqs.len() == 1;
    let mut out = String::new();
    for r in &reqs {
        out.push_str(&fmt_req(r, workspace, full));
        out.push_str("---\n");
    }
    Ok(out)
}

pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
    read_requirements(
        &agent.cfg,
        &agent.workspace,
        opt_i64(args, "id"),
        opt_str(args, "level").as_deref(),
        opt_str(args, "status").as_deref(),
    )
}

pub fn update(agent: &mut Agent, args: &Value) -> Result<String> {
    let id = opt_i64(args, "id").ok_or_else(|| anyhow::anyhow!("missing id"))?;
    let status = opt_str(args, "status");
    if let Some(s) = &status
        && !["active", "met", "removed"].contains(&s.as_str())
    {
        bail!("status must be active|met|removed");
    }
    let level = opt_str(args, "level");
    if let Some(l) = &level
        && l != "stakeholder"
        && l != "system"
    {
        bail!("level must be stakeholder|system");
    }
    let parent_id = args.get("parent_id").map(|v| v.as_i64());
    if !reqmd::update(
        &agent.cfg,
        &agent.workspace,
        id,
        opt_str(args, "title").as_deref(),
        opt_str(args, "body").as_deref(),
        status.as_deref(),
        level.as_deref(),
        parent_id,
    )? {
        bail!("requirement #{id} not found");
    }
    Ok(format!("updated requirement #{id}"))
}

pub fn remove(agent: &mut Agent, args: &Value) -> Result<String> {
    let id = opt_i64(args, "id").ok_or_else(|| anyhow::anyhow!("missing id"))?;
    let hard = args.get("hard").and_then(|v| v.as_bool()).unwrap_or(false);
    if !reqmd::remove(&agent.cfg, &agent.workspace, id, hard)? {
        bail!("requirement #{id} not found");
    }
    Ok(format!(
        "{} requirement #{id}",
        if hard { "deleted" } else { "removed" }
    ))
}

pub fn tree(agent: &mut Agent, args: &Value) -> Result<String> {
    let filter = opt_str(args, "status");
    let reqs: Vec<Requirement> = reqmd::load_all(&agent.cfg, &agent.workspace)?
        .into_iter()
        .filter(|r| filter.as_deref().is_none_or(|s| r.status == s))
        .collect();
    if reqs.is_empty() {
        return Ok("(no requirements)".into());
    }

    // Ticket coverage per requirement id, across open files and archived rows.
    let tickets = super::tickets::list_all(agent)?;
    let mut open: BTreeMap<i64, i64> = BTreeMap::new();
    let mut done: BTreeMap<i64, i64> = BTreeMap::new();
    for t in &tickets {
        if let Some(rid) = t.requirement_id {
            match t.status.as_str() {
                "open" | "in_progress" => *open.entry(rid).or_default() += 1,
                "resolved" | "closed" => *done.entry(rid).or_default() += 1,
                _ => {}
            }
        }
    }

    let ids: BTreeSet<i64> = reqs.iter().map(|r| r.id).collect();
    let mut children: BTreeMap<Option<i64>, Vec<&Requirement>> = BTreeMap::new();
    for r in &reqs {
        // Treat a missing parent as a root so nothing disappears from the tree.
        let parent = match r.parent_id {
            Some(p) if ids.contains(&p) => Some(p),
            _ => None,
        };
        children.entry(parent).or_default().push(r);
    }

    let mut out = String::new();
    out.push_str(&format!(
        "requirements: {} active / {} total, tickets: {} open / {} done\n\n",
        reqs.iter().filter(|r| r.status == "active").count(),
        reqs.len(),
        tickets
            .iter()
            .filter(|t| matches!(t.status.as_str(), "open" | "in_progress"))
            .count(),
        tickets
            .iter()
            .filter(|t| matches!(t.status.as_str(), "resolved" | "closed"))
            .count(),
    ));
    let mut visited = BTreeSet::new();
    if let Some(roots) = children.get(&None) {
        for r in roots {
            render_tree(r, &children, &open, &done, 0, &mut visited, &mut out);
        }
    }
    // Safety net for cycles: show anything not reachable from a root.
    for r in &reqs {
        if !visited.contains(&r.id) {
            render_tree(r, &children, &open, &done, 0, &mut visited, &mut out);
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn render_tree<'a>(
    req: &'a Requirement,
    children: &BTreeMap<Option<i64>, Vec<&'a Requirement>>,
    open: &BTreeMap<i64, i64>,
    done: &BTreeMap<i64, i64>,
    depth: usize,
    visited: &mut BTreeSet<i64>,
    out: &mut String,
) {
    if !visited.insert(req.id) {
        return;
    }
    let indent = "  ".repeat(depth);
    let o = open.get(&req.id).copied().unwrap_or(0);
    let d = done.get(&req.id).copied().unwrap_or(0);
    out.push_str(&format!(
        "{indent}#{} [{}:{}] {}  (tickets: {o} open / {d} done)\n",
        req.id, req.level, req.status, req.title
    ));
    if let Some(kids) = children.get(&Some(req.id)) {
        for k in kids {
            render_tree(k, children, open, done, depth + 1, visited, out);
        }
    }
}

pub fn ask(agent: &mut Agent, args: &Value) -> Result<String> {
    let question = req_str(args, "question")?;
    let requirement_id = opt_i64(args, "requirement_id");
    let qid = agent
        .db
        .question_ask(requirement_id, &agent.instance_id.clone(), &question)?;

    Ok(format!(
        "recorded question #{qid}: {question}\n(no interactive user available; relay this question to the user)"
    ))
}

#[cfg(test)]
mod tests {
    use super::read_requirements;
    use crate::config::Config;
    use crate::reqmd;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("genji-reqtools-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn reads_one_by_id_and_lists_with_filters() {
        let ws = temp_workspace("read");
        let cfg = Config::default();
        let parent = reqmd::create(
            &cfg,
            &ws,
            "stakeholder",
            "Cat Classifier",
            "Must classify cats.",
            None,
            "agent",
        )
        .unwrap();
        let child = reqmd::create(
            &cfg,
            &ws,
            "system",
            "Accept URLs",
            "Accept image URLs.",
            Some(parent.id),
            "agent",
        )
        .unwrap();
        reqmd::update(&cfg, &ws, child.id, None, None, Some("met"), None, None).unwrap();

        // By id: the full requirement body, with no filtering applied.
        let one = read_requirements(&cfg, &ws, Some(parent.id), None, None).unwrap();
        assert!(
            one.contains("#1 [stakeholder:active] Cat Classifier"),
            "{one}"
        );
        assert!(one.contains("Must classify cats."), "{one}");

        // No id: list everything.
        let all = read_requirements(&cfg, &ws, None, None, None).unwrap();
        assert!(
            all.contains("#1 [stakeholder:active] Cat Classifier"),
            "{all}"
        );
        assert!(all.contains("#2 [system:met] Accept URLs"), "{all}");

        // Filter by level.
        let sys = read_requirements(&cfg, &ws, None, Some("system"), None).unwrap();
        assert!(sys.contains("#2 "), "{sys}");
        assert!(!sys.contains("#1 "), "{sys}");

        // Filter by status.
        let met = read_requirements(&cfg, &ws, None, None, Some("met")).unwrap();
        assert!(met.contains("#2 "), "{met}");
        assert!(!met.contains("#1 "), "{met}");

        // A filter with no matches is an empty list, not an error.
        let none = read_requirements(&cfg, &ws, None, None, Some("removed")).unwrap();
        assert_eq!(none, "(no requirements)");

        // A missing id is an error, not an empty list.
        assert!(read_requirements(&cfg, &ws, Some(999), None, None).is_err());

        let _ = std::fs::remove_dir_all(&ws);
    }
}
