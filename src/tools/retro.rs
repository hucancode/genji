use anyhow::{bail, Result};
use rusqlite::params;
use serde_json::Value;

use super::{opt_bool, opt_i64, opt_str, req_str};
use crate::agent::Agent;
use crate::db::PromptVersionRow;
use crate::modes::Mode;

pub fn sessions(agent: &mut Agent, args: &Value) -> Result<String> {
    let limit = opt_i64(args, "limit").unwrap_or(20).clamp(1, 500);
    let mode = opt_str(args, "mode");
    let mut sql = String::from(
        "SELECT id,mode,parent_session,depth,status,tokens_used,started_at,substr(COALESCE(task,''),1,80) FROM sessions WHERE 1=1",
    );
    let mut params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(m) = mode {
        sql.push_str(" AND mode=?");
        params_vec.push(Box::new(m));
    }
    sql.push_str(" ORDER BY started_at DESC LIMIT ?");
    params_vec.push(Box::new(limit));
    let mut stmt = agent.db.conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::types::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        Ok(format!(
            "{}\t{}\tdepth={}\t{}\ttokens={}\t{}\t{}\n",
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?
        ))
    })?;
    let mut out = String::new();
    for row in rows {
        out.push_str(&row?);
    }
    if out.is_empty() {
        out = "(no sessions)".into();
    }
    Ok(out)
}

pub fn session(agent: &mut Agent, args: &Value) -> Result<String> {
    let sid = req_str(args, "session_id")?;
    let limit = opt_i64(args, "limit").unwrap_or(200).clamp(1, 2000);
    let mut stmt = agent.db.conn.prepare(
        "SELECT seq,role,content,tool_calls FROM messages WHERE session_id=? ORDER BY seq LIMIT ?",
    )?;
    let rows = stmt.query_map(params![sid, limit], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    let mut out = String::new();
    for row in rows {
        let (seq, role, content, tc) = row?;
        let snippet: String = content.chars().take(500).collect();
        out.push_str(&format!("#{seq} [{role}] {snippet}\n"));
        if let Some(tc) = tc {
            if tc != "null" && !tc.is_empty() {
                out.push_str(&format!("    tool_calls: {}\n", tc.chars().take(200).collect::<String>()));
            }
        }
    }
    if out.is_empty() {
        out = format!("(session {sid} has no messages)");
    }
    Ok(out)
}

pub fn messages(agent: &mut Agent, args: &Value) -> Result<String> {
    let limit = opt_i64(args, "limit").unwrap_or(50).clamp(1, 500);
    let mut sql = String::from("SELECT session_id,seq,role,substr(content,1,400),created_at FROM messages WHERE 1=1");
    let mut p: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(s) = opt_str(args, "session_id") {
        sql.push_str(" AND session_id=?");
        p.push(Box::new(s));
    }
    if let Some(r) = opt_str(args, "role") {
        sql.push_str(" AND role=?");
        p.push(Box::new(r));
    }
    if let Some(q) = opt_str(args, "search") {
        sql.push_str(" AND content LIKE ?");
        p.push(Box::new(format!("%{q}%")));
    }
    sql.push_str(" ORDER BY id DESC LIMIT ?");
    p.push(Box::new(limit));
    let mut stmt = agent.db.conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::types::ToSql> = p.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        Ok(format!(
            "{}\t#{} [{}]\t{}\t{}\n",
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(3)?
        ))
    })?;
    let mut out = String::new();
    for row in rows {
        out.push_str(&row?);
    }
    if out.is_empty() {
        out = "(no matching messages)".into();
    }
    Ok(out)
}

pub fn tool_calls(agent: &mut Agent, args: &Value) -> Result<String> {
    let limit = opt_i64(args, "limit").unwrap_or(50).clamp(1, 500);
    let mut sql = String::from(
        "SELECT session_id,name,is_error,duration_ms,substr(args,1,160),substr(result,1,300),created_at FROM tool_calls WHERE 1=1",
    );
    let mut p: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(s) = opt_str(args, "session_id") {
        sql.push_str(" AND session_id=?");
        p.push(Box::new(s));
    }
    if let Some(n) = opt_str(args, "name") {
        sql.push_str(" AND name=?");
        p.push(Box::new(n));
    }
    if opt_bool(args, "errors_only").unwrap_or(false) {
        sql.push_str(" AND is_error=1");
    }
    sql.push_str(" ORDER BY id DESC LIMIT ?");
    p.push(Box::new(limit));
    let mut stmt = agent.db.conn.prepare(&sql)?;
    let refs: Vec<&dyn rusqlite::types::ToSql> = p.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |r| {
        Ok(format!(
            "{}\t{}\terr={}\t{}ms\t{}\n  args: {}\n  result: {}\n",
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, i64>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    let mut out = String::new();
    for row in rows {
        out.push_str(&row?);
    }
    if out.is_empty() {
        out = "(no matching tool calls)".into();
    }
    Ok(out)
}

pub fn stats(agent: &mut Agent, _args: &Value) -> Result<String> {
    let mut out = String::new();
    let (sessions, tokens): (i64, i64) = agent.db.conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(tokens_used),0) FROM sessions",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    out.push_str(&format!("sessions: {sessions}, total tokens: {tokens}\n"));
    out.push_str("sessions by mode:\n");
    {
        let mut stmt = agent
            .db
            .conn
            .prepare("SELECT mode,COUNT(*) FROM sessions GROUP BY mode ORDER BY 2 DESC")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        for row in rows {
            let (m, c) = row?;
            out.push_str(&format!("  {m}: {c}\n"));
        }
    }
    out.push_str("tool calls (name, count, errors, avg_ms):\n");
    {
        let mut stmt = agent.db.conn.prepare(
            "SELECT name,COUNT(*),COALESCE(SUM(is_error),0),CAST(AVG(duration_ms) AS INT)
             FROM tool_calls GROUP BY name ORDER BY 2 DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        for row in rows {
            let (n, c, e, d) = row?;
            out.push_str(&format!("  {n}: calls={c} errors={e} avg={d}ms\n"));
        }
    }
    out.push_str("skills by loads:\n");
    {
        let mut stmt = agent.db.conn.prepare(
            "SELECT skill_name,COUNT(*) FROM skill_loads GROUP BY skill_name ORDER BY 2 DESC LIMIT 20",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        let mut any = false;
        for row in rows {
            let (n, c) = row?;
            any = true;
            out.push_str(&format!("  {n}: {c}\n"));
        }
        if !any {
            out.push_str("  (none)\n");
        }
    }
    let compactions: i64 =
        agent
            .db
            .conn
            .query_row("SELECT COUNT(*) FROM compactions", [], |r| r.get(0))?;
    out.push_str(&format!("compactions: {compactions}\n"));
    Ok(out)
}

// ------------------------------------------------------------------- prompts

fn parse_mode(s: &str) -> Result<Mode> {
    let mode = Mode::parse(s)?;
    if !mode.allows_extended() {
        bail!(
            "mode `{}` does not support an extended prompt",
            mode.as_str()
        );
    }
    Ok(mode)
}

pub fn prompt_read(agent: &mut Agent, args: &Value) -> Result<String> {
    let mode = parse_mode(&req_str(args, "mode")?)?;
    match agent.db.prompt_active(mode.as_str())? {
        Some(p) => Ok(format!(
            "mode={} version={} author={} created={}\n\n{}",
            mode.as_str(),
            p.version,
            p.author,
            p.created_at,
            p.content
        )),
        None => Ok(format!("(no active extended prompt for {})", mode.as_str())),
    }
}

pub fn prompt_edit(agent: &mut Agent, args: &Value) -> Result<String> {
    let mode = parse_mode(&req_str(args, "mode")?)?;
    let content = req_str(args, "content")?;
    let reason = opt_str(args, "reason").unwrap_or_default();
    let version = agent
        .db
        .prompt_add_version(mode.as_str(), &content, "retro", &reason)?;
    crate::prompts::write_prompt_file(&agent.cfg, &agent.workspace, mode, &content)?;
    // Keep the in-memory system prompt fresh if we edited our own mode.
    agent.refresh_system_prompt()?;
    Ok(format!(
        "updated extended prompt for `{}` to v{version}",
        mode.as_str()
    ))
}

pub fn prompt_history(agent: &mut Agent, args: &Value) -> Result<String> {
    let mode = parse_mode(&req_str(args, "mode")?)?;
    let versions: Vec<PromptVersionRow> = agent.db.prompt_versions(mode.as_str())?;
    if versions.is_empty() {
        return Ok(format!("(no versions for {})", mode.as_str()));
    }
    let mut out = String::new();
    for v in versions {
        out.push_str(&format!(
            "v{}\t{}\tactive={}\tby {}\t{}\t{}B\n",
            v.version,
            v.created_at,
            v.active,
            v.author,
            v.reason,
            v.content.len()
        ));
    }
    Ok(out)
}

pub fn prompt_rollback(agent: &mut Agent, args: &Value) -> Result<String> {
    let mode = parse_mode(&req_str(args, "mode")?)?;
    let version = opt_i64(args, "version").ok_or_else(|| anyhow::anyhow!("missing version"))?;
    let versions = agent.db.prompt_versions(mode.as_str())?;
    let target = versions
        .iter()
        .find(|v| v.version == version)
        .ok_or_else(|| anyhow::anyhow!("mode {} has no version {}", mode.as_str(), version))?;
    let content = target.content.clone();
    if !agent.db.prompt_activate(mode.as_str(), version)? {
        bail!("could not activate version {version}");
    }
    crate::prompts::write_prompt_file(&agent.cfg, &agent.workspace, mode, &content)?;
    agent.refresh_system_prompt()?;
    Ok(format!(
        "activated v{version} for `{}` extended prompt",
        mode.as_str()
    ))
}
