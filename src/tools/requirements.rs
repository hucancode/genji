use anyhow::{bail, Result};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{opt_i64, opt_str, req_str};
use crate::agent::Agent;
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
        r.id, r.title, r.display_path(&agent.workspace)
    ))
}

pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
    if let Some(id) = opt_i64(args, "id") {
        return match reqmd::load_by_id(&agent.cfg, &agent.workspace, id)? {
            Some(r) => Ok(fmt_req(&r, &agent.workspace, true)),
            None => bail!("requirement #{id} not found"),
        };
    }
    let level = opt_str(args, "level");
    let status = opt_str(args, "status");
    let reqs: Vec<Requirement> = reqmd::load_all(&agent.cfg, &agent.workspace)?
        .into_iter()
        .filter(|r| level.as_deref().is_none_or(|l| r.level == l))
        .filter(|r| status.as_deref().is_none_or(|s| r.status == s))
        .collect();
    if reqs.is_empty() {
        return Ok("(no requirements)".into());
    }
    let full = reqs.len() == 1;
    let mut out = String::new();
    for r in &reqs {
        out.push_str(&fmt_req(r, &agent.workspace, full));
        out.push_str("---\n");
    }
    Ok(out)
}

pub fn update(agent: &mut Agent, args: &Value) -> Result<String> {
    let id = opt_i64(args, "id").ok_or_else(|| anyhow::anyhow!("missing id"))?;
    let status = opt_str(args, "status");
    if let Some(s) = &status {
        if !["active", "met", "removed"].contains(&s.as_str()) {
            bail!("status must be active|met|removed");
        }
    }
    let level = opt_str(args, "level");
    if let Some(l) = &level {
        if l != "stakeholder" && l != "system" {
            bail!("level must be stakeholder|system");
        }
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

    // Ticket coverage per requirement id.
    let tickets = agent.db.ticket_list(None, None)?;
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

    if agent.interactive {
        eprintln!("\n[requirement_ask] {question}\n> answer (blank to skip):");
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok() {
            let answer = line.trim().to_string();
            if !answer.is_empty() {
                agent.db.question_answer(qid, &answer)?;
                return Ok(format!("answer: {answer}"));
            }
        }
    }
    Ok(format!(
        "recorded question #{qid}: {question}\n(no interactive user available; relay this question to the user)"
    ))
}
