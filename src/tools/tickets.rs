use anyhow::{Result, bail};
use serde_json::Value;
use std::path::Path;

use super::{opt_i64, opt_str, req_str};
use crate::agent::Agent;
use crate::db::TicketEdit;
use crate::ticketmd::{self, Ticket};

fn find(agent: &Agent, id: i64) -> Result<Option<Ticket>> {
    ticketmd::load_by_id(&agent.cfg, &agent.workspace, id)
}

pub fn list_all(agent: &Agent) -> Result<Vec<Ticket>> {
    let mut all = ticketmd::load_all(&agent.cfg, &agent.workspace)?;
    all.sort_by_key(|t| (t.priority, t.id));
    Ok(all)
}

/// Format one ticket for tool output.
fn fmt_ticket(t: &Ticket, workspace: &Path) -> String {
    let mut s = format!("#{} [{}] {}\n", t.id, t.status, t.title);
    if t.priority != 2 {
        s.push_str(&format!("priority: {}\n", t.priority));
    }
    if let Some(r) = t.requirement_id {
        s.push_str(&format!("requirement: #{r}\n"));
    }
    if let Some(p) = t.parent_id {
        s.push_str(&format!("parent: #{p}\n"));
    }
    if let Some(path) = t.display_path(workspace) {
        s.push_str(&format!("path: {path}\n"));
    }
    s.push_str(&format!("created: {}\n", t.created_at));
    if let Some(r) = &t.resolution {
        s.push_str(&format!("resolution: {r}\n"));
    }
    if !t.description.trim().is_empty() {
        s.push('\n');
        s.push_str(t.description.trim());
        s.push('\n');
    }
    s
}

pub fn create(agent: &mut Agent, args: &Value) -> Result<String> {
    let title = req_str(args, "title")?;
    let description = opt_str(args, "description").unwrap_or_default();
    let priority = opt_i64(args, "priority").unwrap_or(2).clamp(1, 3);
    let parent_id = opt_i64(args, "parent_id");
    let requirement_id = opt_i64(args, "requirement_id");
    let mode = agent.mode.as_str().to_string();
    let t = ticketmd::create(
        &agent.cfg,
        &agent.workspace,
        &title,
        &description,
        priority,
        parent_id,
        requirement_id,
        &mode,
    )?;
    Ok(format!("created ticket #{}: {}", t.id, t.title))
}

pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
    if let Some(id) = opt_i64(args, "id") {
        return match find(agent, id)? {
            Some(t) => Ok(fmt_ticket(&t, &agent.workspace)),
            None => bail!("ticket #{id} not found"),
        };
    }
    let status = opt_str(args, "status");
    let requirement_id = opt_i64(args, "requirement_id");
    // `ticket_read` owns listing: with no id it lists the actionable,
    // file-backed (open/in_progress) tickets. Archived tickets are still
    // reachable by id above.
    let mut tickets = ticketmd::load_all(&agent.cfg, &agent.workspace)?;
    tickets.sort_by_key(|t| (t.priority, t.id));
    let tickets: Vec<Ticket> = tickets
        .into_iter()
        .filter(|t| status.as_deref().is_none_or(|s| t.status == s))
        .filter(|t| requirement_id.is_none_or(|r| t.requirement_id == Some(r)))
        .collect();
    if tickets.is_empty() {
        return Ok("(no tickets)".into());
    }
    let mut out = String::new();
    for t in &tickets {
        out.push_str(&fmt_ticket(t, &agent.workspace));
        out.push_str("---\n");
    }
    Ok(out)
}

pub fn claim(agent: &mut Agent, args: &Value) -> Result<String> {
    let id = opt_i64(args, "id");
    let requirement_id = opt_i64(args, "requirement_id");
    let ticket = match id {
        Some(id) => find(agent, id)?,
        None => list_all(agent)?.into_iter().find(|t| {
            t.status == "open" && requirement_id.is_none_or(|r| t.requirement_id == Some(r))
        }),
    };
    let Some(t) = ticket else {
        return Ok(match id {
            Some(id) => format!("(no claimable ticket #{id})"),
            None => "(no open tickets to claim)".to_string(),
        });
    };
    if !matches!(t.status.as_str(), "open" | "in_progress") {
        bail!("ticket #{} is {} and cannot be claimed", t.id, t.status);
    }
    let Some(t) = ticketmd::update(
        &agent.cfg,
        &agent.workspace,
        t.id,
        &TicketEdit::default(),
        Some("in_progress"),
        None,
    )?
    else {
        bail!("ticket #{} is archived and cannot be claimed", t.id);
    };
    Ok(format!(
        "claimed ticket #{}\n\n{}",
        t.id,
        fmt_ticket(&t, &agent.workspace)
    ))
}

pub fn update(agent: &mut Agent, args: &Value) -> Result<String> {
    let id = opt_i64(args, "id").ok_or_else(|| anyhow::anyhow!("missing id"))?;
    let status = opt_str(args, "status");
    if let Some(s) = &status
        && !["open", "in_progress", "resolved", "closed"].contains(&s.as_str())
    {
        bail!("status must be open|in_progress|resolved|closed");
    }
    let edit = TicketEdit {
        title: opt_str(args, "title"),
        description: opt_str(args, "description"),
        priority: opt_i64(args, "priority").map(|p| p.clamp(1, 3)),
        parent_id: args.get("parent_id").map(|v| v.as_i64()),
        requirement_id: args.get("requirement_id").map(|v| v.as_i64()),
    };
    let resolution = opt_str(args, "resolution");
    let Some(_t) = find(agent, id)? else {
        bail!("ticket #{id} not found");
    };
    ticketmd::update(
        &agent.cfg,
        &agent.workspace,
        id,
        &edit,
        status.as_deref(),
        resolution.as_deref(),
    )?
    .ok_or_else(|| anyhow::anyhow!("ticket #{id} disappeared during update"))?;
    Ok(format!("updated ticket #{id}"))
}

pub fn close(agent: &mut Agent, args: &Value) -> Result<String> {
    let id = opt_i64(args, "id").ok_or_else(|| anyhow::anyhow!("missing id"))?;
    let reason = opt_str(args, "reason");
    let Some(_t) = find(agent, id)? else {
        bail!("ticket #{id} not found");
    };
    ticketmd::update(
        &agent.cfg,
        &agent.workspace,
        id,
        &TicketEdit::default(),
        Some("closed"),
        reason.as_deref(),
    )?;
    Ok(format!("ticket #{id} closed"))
}
