use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::Path;

use super::{opt_i64, opt_str, req_str};
use crate::agent::Agent;
use crate::db::Db;

/// Parse an optional YAML-ish frontmatter block. Returns (name, description, body).
pub fn parse_skill(text: &str, fallback_name: &str) -> (String, String, String) {
    let mut name = fallback_name.to_string();
    let mut description = String::new();
    let body;
    if let Some(rest) = text.strip_prefix("---\n")
        && let Some(idx) = rest.find("\n---")
    {
        let front = &rest[..idx];
        for line in front.lines() {
            if let Some(v) = line.strip_prefix("name:") {
                name = v.trim().trim_matches('"').to_string();
            } else if let Some(v) = line.strip_prefix("description:") {
                description = v.trim().trim_matches('"').to_string();
            }
        }
        body = rest[idx + 4..].trim_start_matches('\n').to_string();
        return (name, description, body);
    }
    body = text.to_string();
    (name, description, body)
}

fn render_skill(name: &str, description: &str, content: &str) -> String {
    format!(
        "---\nname: {name}\ndescription: {}\n---\n\n{}\n",
        description.replace('\n', " "),
        content.trim_end()
    )
}

/// Scan the skills directory and sync markdown files into the database,
/// recording a version whenever content changes.
pub fn sync_skills(db: &Db, dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string();
        let text = std::fs::read_to_string(&path)?;
        let (name, desc, body) = parse_skill(&text, &stem);
        let existing = db.skill_get(&name)?;
        let changed = match &existing {
            Some(e) => e.content != body || e.description != desc,
            None => true,
        };
        db.skill_upsert(&name, &path.to_string_lossy(), &desc, &body)?;
        if changed {
            db.skill_version_add(&name, &body, &desc, "file", "synced from file")?;
        }
        count += 1;
    }
    Ok(count)
}

fn write_skill_file(agent: &Agent, name: &str, description: &str, content: &str) -> Result<()> {
    let dir = agent.cfg.skills_path(&agent.workspace);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{name}.md"));
    std::fs::write(&path, render_skill(name, description, content))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn save_skill(
    agent: &Agent,
    name: &str,
    description: &str,
    content: &str,
    author: &str,
    reason: &str,
) -> Result<()> {
    let dir = agent.cfg.skills_path(&agent.workspace);
    let path = dir.join(format!("{name}.md"));
    agent
        .db
        .skill_upsert(name, &path.to_string_lossy(), description, content)?;
    agent
        .db
        .skill_version_add(name, content, description, author, reason)?;
    write_skill_file(agent, name, description, content)?;
    Ok(())
}

pub fn load(agent: &mut Agent, args: &Value) -> Result<String> {
    let name = req_str(args, "name")?;
    let skill = agent.db.skill_get(&name)?;
    let instance_id = agent.instance_id.clone();
    match skill {
        Some(s) => {
            agent.db.skill_record_load(&instance_id, &name)?;
            Ok(format!(
                "# Skill: {}\n{}\n\n{}",
                s.name, s.description, s.content
            ))
        }
        None => {
            let names: Vec<String> = agent.db.skill_list()?.into_iter().map(|s| s.name).collect();
            bail!(
                "skill `{name}` not found. available: {}",
                if names.is_empty() {
                    "(none)".into()
                } else {
                    names.join(", ")
                }
            )
        }
    }
}

pub fn list(agent: &mut Agent, _args: &Value) -> Result<String> {
    let skills = agent.db.skill_list()?;
    if skills.is_empty() {
        return Ok("(no skills)".into());
    }
    let mut out = String::new();
    for s in skills {
        out.push_str(&format!(
            "- {} (uses={}): {}\n",
            s.name,
            s.uses,
            if s.description.is_empty() {
                "(no description)"
            } else {
                &s.description
            }
        ));
    }
    Ok(out)
}

pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
    let name = req_str(args, "name")?;
    if let Some(v) = opt_i64(args, "version") {
        return match agent.db.skill_version_get(&name, v)? {
            Some((content, desc)) => Ok(format!("# {name} v{v}\n{desc}\n\n{content}")),
            None => bail!("skill `{name}` has no version {v}"),
        };
    }
    let s = agent
        .db
        .skill_get(&name)?
        .ok_or_else(|| anyhow::anyhow!("skill `{name}` not found"))?;
    Ok(format!(
        "# {}\npath: {}\nuses: {}\ndescription: {}\n\n{}",
        s.name, s.path, s.uses, s.description, s.content
    ))
}

pub fn write(agent: &mut Agent, args: &Value) -> Result<String> {
    let name = req_str(args, "name")?;
    if name.contains('/') || name.contains("..") {
        bail!("invalid skill name");
    }
    let description = opt_str(args, "description").unwrap_or_default();
    let content = req_str(args, "content")?;
    let reason = opt_str(args, "reason").unwrap_or_default();
    let existed = agent.db.skill_get(&name)?.is_some();
    save_skill(agent, &name, &description, &content, "retro", &reason)?;
    Ok(format!(
        "{} skill `{name}` ({} bytes)",
        if existed { "updated" } else { "created" },
        content.len()
    ))
}

pub fn edit(agent: &mut Agent, args: &Value) -> Result<String> {
    let name = req_str(args, "name")?;
    let old = req_str(args, "oldText")?;
    let new = req_str(args, "newText")?;
    let reason = opt_str(args, "reason").unwrap_or_default();
    let s = agent
        .db
        .skill_get(&name)?
        .ok_or_else(|| anyhow::anyhow!("skill `{name}` not found"))?;
    let count = s.content.matches(&old).count();
    if count == 0 {
        bail!("oldText not found in skill `{name}`");
    }
    if count > 1 {
        bail!("oldText is not unique in skill `{name}` ({count} matches)");
    }
    let updated = s.content.replacen(&old, &new, 1);
    save_skill(agent, &name, &s.description, &updated, "retro", &reason)?;
    Ok(format!("edited skill `{name}`"))
}

pub fn history(agent: &mut Agent, args: &Value) -> Result<String> {
    let name = req_str(args, "name")?;
    let mut stmt = agent.db.conn.prepare(
        "SELECT version,author,reason,length(content),created_at FROM skill_versions
         WHERE skill_name=? ORDER BY version DESC",
    )?;
    let rows = stmt.query_map(rusqlite::params![name], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, i64>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = String::new();
    for row in rows {
        let (v, author, reason, len, at) = row?;
        out.push_str(&format!("v{v}\t{at}\tby {author}\t{len}B\t{reason}\n"));
    }
    if out.is_empty() {
        out = format!("(no versions for `{name}`)");
    }
    Ok(out)
}

pub fn rollback(agent: &mut Agent, args: &Value) -> Result<String> {
    let name = req_str(args, "name")?;
    let version = opt_i64(args, "version").ok_or_else(|| anyhow::anyhow!("missing version"))?;
    let (content, desc) = agent
        .db
        .skill_version_get(&name, version)?
        .ok_or_else(|| anyhow::anyhow!("skill `{name}` has no version {version}"))?;
    save_skill(
        agent,
        &name,
        &desc,
        &content,
        "rollback",
        &format!("rolled back to v{version}"),
    )?;
    Ok(format!("rolled skill `{name}` back to v{version}"))
}

#[cfg(test)]
mod tests {
    use super::parse_skill;

    #[test]
    fn parses_frontmatter() {
        let text = "---\nname: foo\ndescription: bar baz\n---\n\n# Body\ncontent\n";
        let (n, d, b) = parse_skill(text, "fallback");
        assert_eq!(n, "foo");
        assert_eq!(d, "bar baz");
        assert!(b.contains("Body"));
    }

    #[test]
    fn falls_back_without_frontmatter() {
        let (n, _d, b) = parse_skill("just content", "stem");
        assert_eq!(n, "stem");
        assert_eq!(b, "just content");
    }
}
