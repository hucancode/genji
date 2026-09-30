use anyhow::{bail, Result};
use serde_json::Value;

use super::{opt_i64, opt_str, req_str};
use crate::agent::Agent;
use crate::db::Ticket;

fn fmt_ticket(t: &Ticket) -> String {
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
    let id = agent.db.ticket_create(
        &title,
        &description,
        priority,
        parent_id,
        requirement_id,
        &mode,
    )?;
    Ok(format!("created ticket #{id}: {title}"))
}

pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
    if let Some(id) = opt_i64(args, "id") {
        return match agent.db.ticket_get(id)? {
            Some(t) => Ok(fmt_ticket(&t)),
            None => bail!("ticket #{id} not found"),
        };
    }
    let status = opt_str(args, "status");
    let requirement_id = opt_i64(args, "requirement_id");
    let tickets = agent.db.ticket_list(status.as_deref(), requirement_id)?;
    if tickets.is_empty() {
        return Ok("(no tickets)".into());
    }
    let mut out = String::new();
    for t in &tickets {
        out.push_str(&fmt_ticket(t));
        out.push_str("---\n");
    }
    Ok(out)
}

pub fn resolve(agent: &mut Agent, args: &Value) -> Result<String> {
    let id = opt_i64(args, "id").ok_or_else(|| anyhow::anyhow!("missing id"))?;
    let resolution = opt_str(args, "resolution");
    if !agent
        .db
        .ticket_set_status(id, "resolved", resolution.as_deref())?
    {
        bail!("ticket #{id} not found");
    }
    Ok(format!("ticket #{id} resolved"))
}

pub fn close(agent: &mut Agent, args: &Value) -> Result<String> {
    let id = opt_i64(args, "id").ok_or_else(|| anyhow::anyhow!("missing id"))?;
    let reason = opt_str(args, "reason");
    if !agent.db.ticket_set_status(id, "closed", reason.as_deref())? {
        bail!("ticket #{id} not found");
    }
    Ok(format!("ticket #{id} closed"))
}
