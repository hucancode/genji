use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// A partial edit to a ticket. `None` leaves a field untouched; for the two
/// nullable links `Some(None)` clears the value.
#[cfg(feature = "formal")]
#[derive(Default)]
pub struct TicketEdit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub parent_id: Option<Option<i64>>,
    pub requirement_id: Option<Option<i64>>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct SkillRow {
    pub id: i64,
    pub name: String,
    pub path: String,
    pub description: String,
    pub content: String,
    pub uses: i64,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct PromptVersionRow {
    pub id: i64,
    pub mode: String,
    pub version: i64,
    pub content: String,
    pub author: String,
    pub reason: String,
    pub active: i64,
    pub created_at: String,
}

pub struct Db {
    pub conn: Connection,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating db dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening sqlite db {}", path.display()))?;
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
        )?;
        Ok(Db { conn })
    }

    pub fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(include_str!("sql/schema.sql"))?;
        Ok(())
    }

    // ------------------------------------------------------------- instances

    pub fn instance_start(
        &self,
        id: &str,
        mode: &str,
        parent: Option<&str>,
        task: &str,
        model: &str,
        depth: u32,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO instances(id,mode,parent_instance,task,model,depth,status) VALUES(?,?,?,?,?,?,'running')",
            params![id, mode, parent, task, model, depth as i64],
        )?;
        Ok(())
    }

    pub fn instance_end(&self, id: &str, status: &str, tokens: i64, report: &str) -> Result<()> {
        self.conn.execute(
            &format!(
                "UPDATE instances SET status=?, tokens_used=?, report=?, ended_at={NOW} WHERE id=?"
            ),
            params![status, tokens, report, id],
        )?;
        Ok(())
    }

    pub fn instance_set_tokens(&self, id: &str, tokens: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE instances SET tokens_used=? WHERE id=?",
            params![tokens, id],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn message_add(
        &self,
        instance_id: &str,
        seq: i64,
        role: &str,
        content: &str,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        reasoning: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO messages(instance_id,seq,role,content,tool_calls,tool_call_id,reasoning) VALUES(?,?,?,?,?,?,?)",
            params![instance_id, seq, role, content, tool_calls, tool_call_id, reasoning],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn tool_call_add(
        &self,
        instance_id: &str,
        message_seq: i64,
        name: &str,
        args: &str,
        result: &str,
        is_error: bool,
        duration_ms: i64,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tool_calls(instance_id,message_seq,name,args,result,is_error,duration_ms) VALUES(?,?,?,?,?,?,?)",
            params![instance_id, message_seq, name, args, result, is_error as i64, duration_ms],
        )?;
        Ok(())
    }

    #[cfg(feature = "formal")]
    pub fn question_ask(
        &self,
        requirement_id: Option<i64>,
        instance_id: &str,
        question: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO requirement_questions(requirement_id,instance_id,question) VALUES(?,?,?)",
            params![requirement_id, instance_id, question],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    // ---------------------------------------------------------------- skills

    pub fn skill_upsert(
        &self,
        name: &str,
        path: &str,
        description: &str,
        content: &str,
    ) -> Result<i64> {
        self.conn.execute(
            &format!(
                "INSERT INTO skills(name,path,description,content,uses) VALUES(?,?,?,?,0)
                 ON CONFLICT(name) DO UPDATE SET path=excluded.path, description=excluded.description,
                    content=excluded.content, updated_at={NOW}"
            ),
            params![name, path, description, content],
        )?;
        let id: i64 =
            self.conn
                .query_row("SELECT id FROM skills WHERE name=?", params![name], |r| {
                    r.get(0)
                })?;
        Ok(id)
    }

    pub fn skill_get(&self, name: &str) -> Result<Option<SkillRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,name,path,description,content,uses FROM skills WHERE name=?",
                params![name],
                |r| {
                    Ok(SkillRow {
                        id: r.get(0)?,
                        name: r.get(1)?,
                        path: r.get(2)?,
                        description: r.get(3)?,
                        content: r.get(4)?,
                        uses: r.get(5)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn skill_list(&self) -> Result<Vec<SkillRow>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id,name,path,description,content,uses FROM skills ORDER BY name")?;
        let rows = stmt.query_map([], |r| {
            Ok(SkillRow {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                description: r.get(3)?,
                content: r.get(4)?,
                uses: r.get(5)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn skill_record_load(&self, instance_id: &str, name: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO skill_loads(instance_id,skill_name) VALUES(?,?)",
            params![instance_id, name],
        )?;
        self.conn
            .execute("UPDATE skills SET uses=uses+1 WHERE name=?", params![name])?;
        Ok(())
    }

    pub fn skill_version_add(
        &self,
        name: &str,
        content: &str,
        description: &str,
        author: &str,
        reason: &str,
    ) -> Result<i64> {
        let version: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(version),0)+1 FROM skill_versions WHERE skill_name=?",
            params![name],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO skill_versions(skill_name,version,content,description,author,reason) VALUES(?,?,?,?,?,?)",
            params![name, version, content, description, author, reason],
        )?;
        Ok(version)
    }

    pub fn skill_version_get(&self, name: &str, version: i64) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT content,description FROM skill_versions WHERE skill_name=? AND version=?",
                params![name, version],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    // --------------------------------------------------------- prompts

    pub fn prompt_active(&self, mode: &str) -> Result<Option<PromptVersionRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id,mode,version,content,author,reason,active,created_at FROM prompt_versions
                 WHERE mode=? AND active=1 ORDER BY version DESC LIMIT 1",
                params![mode],
                |r| {
                    Ok(PromptVersionRow {
                        id: r.get(0)?,
                        mode: r.get(1)?,
                        version: r.get(2)?,
                        content: r.get(3)?,
                        author: r.get(4)?,
                        reason: r.get(5)?,
                        active: r.get(6)?,
                        created_at: r.get(7)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn prompt_add_version(
        &self,
        mode: &str,
        content: &str,
        author: &str,
        reason: &str,
    ) -> Result<i64> {
        let version: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(version),0)+1 FROM prompt_versions WHERE mode=?",
            params![mode],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "UPDATE prompt_versions SET active=0 WHERE mode=?",
            params![mode],
        )?;
        self.conn.execute(
            "INSERT INTO prompt_versions(mode,version,content,author,reason,active) VALUES(?,?,?,?,?,1)",
            params![mode, version, content, author, reason],
        )?;
        Ok(version)
    }

    pub fn prompt_activate(&self, mode: &str, version: i64) -> Result<bool> {
        let exists: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM prompt_versions WHERE mode=? AND version=?",
                params![mode, version],
                |r| r.get(0),
            )
            .optional()?;
        if exists.is_none() {
            return Ok(false);
        }
        self.conn.execute(
            "UPDATE prompt_versions SET active=0 WHERE mode=?",
            params![mode],
        )?;
        self.conn.execute(
            "UPDATE prompt_versions SET active=1 WHERE mode=? AND version=?",
            params![mode, version],
        )?;
        Ok(true)
    }

    pub fn prompt_versions(&self, mode: &str) -> Result<Vec<PromptVersionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id,mode,version,content,author,reason,active,created_at FROM prompt_versions
             WHERE mode=? ORDER BY version DESC",
        )?;
        let rows = stmt.query_map(params![mode], |r| {
            Ok(PromptVersionRow {
                id: r.get(0)?,
                mode: r.get(1)?,
                version: r.get(2)?,
                content: r.get(3)?,
                author: r.get(4)?,
                reason: r.get(5)?,
                active: r.get(6)?,
                created_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // --------------------------------------------------------- compaction log

    pub fn compaction_add(
        &self,
        instance_id: &str,
        removed: i64,
        before: i64,
        after: i64,
        summary: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO compactions(instance_id,removed_messages,before_tokens,after_tokens,summary) VALUES(?,?,?,?,?)",
            params![instance_id, removed, before, after, summary],
        )?;
        Ok(())
    }
}
