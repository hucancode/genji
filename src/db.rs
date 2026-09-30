use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

#[allow(dead_code)]
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
    pub resolved_at: Option<String>,
    pub closed_at: Option<String>,
}

/// A partial edit to a ticket. `None` leaves a field untouched; for the two
/// nullable links `Some(None)` clears the value.
pub struct TicketEdit {
    pub title: Option<String>,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub parent_id: Option<Option<i64>>,
    pub requirement_id: Option<Option<i64>>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct Requirement {
    pub id: i64,
    pub level: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub parent_id: Option<i64>,
    pub source: String,
    pub source_path: Option<String>,
    pub created_at: String,
    pub updated_at: String,
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

fn t_row(r: &Row) -> rusqlite::Result<Ticket> {
    Ok(Ticket {
        id: r.get(0)?,
        title: r.get(1)?,
        description: r.get(2)?,
        status: r.get(3)?,
        priority: r.get(4)?,
        parent_id: r.get(5)?,
        requirement_id: r.get(6)?,
        mode: r.get(7)?,
        resolution: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
        resolved_at: r.get(11)?,
        closed_at: r.get(12)?,
    })
}

const TICKET_COLS: &str = "id,title,description,status,priority,parent_id,requirement_id,mode,resolution,created_at,updated_at,resolved_at,closed_at";

fn r_row(r: &Row) -> rusqlite::Result<Requirement> {
    Ok(Requirement {
        id: r.get(0)?,
        level: r.get(1)?,
        title: r.get(2)?,
        body: r.get(3)?,
        status: r.get(4)?,
        parent_id: r.get(5)?,
        source: r.get(6)?,
        source_path: r.get(7)?,
        created_at: r.get(8)?,
        updated_at: r.get(9)?,
    })
}

const REQ_COLS: &str =
    "id,level,title,body,status,parent_id,source,source_path,created_at,updated_at";

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
        self.migrate_session_to_instance()?;
        self.conn.execute_batch(&format!(
            r#"
CREATE TABLE IF NOT EXISTS tickets (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  title TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'open',
  priority INTEGER NOT NULL DEFAULT 2,
  parent_id INTEGER,
  requirement_id INTEGER,
  mode TEXT,
  resolution TEXT,
  created_at TEXT NOT NULL DEFAULT ({NOW}),
  updated_at TEXT NOT NULL DEFAULT ({NOW}),
  resolved_at TEXT,
  closed_at TEXT
);
CREATE INDEX IF NOT EXISTS idx_tickets_status ON tickets(status);
CREATE INDEX IF NOT EXISTS idx_tickets_req ON tickets(requirement_id);

CREATE TABLE IF NOT EXISTS requirements (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  level TEXT NOT NULL CHECK(level IN ('stakeholder','system')),
  title TEXT NOT NULL,
  body TEXT NOT NULL DEFAULT '',
  status TEXT NOT NULL DEFAULT 'active',
  parent_id INTEGER,
  source TEXT NOT NULL DEFAULT 'agent',
  source_path TEXT,
  created_at TEXT NOT NULL DEFAULT ({NOW}),
  updated_at TEXT NOT NULL DEFAULT ({NOW})
);
CREATE INDEX IF NOT EXISTS idx_req_status ON requirements(status);
CREATE INDEX IF NOT EXISTS idx_req_level ON requirements(level);
CREATE UNIQUE INDEX IF NOT EXISTS idx_req_source_path ON requirements(source_path)
  WHERE source_path IS NOT NULL;

CREATE TABLE IF NOT EXISTS requirement_questions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  requirement_id INTEGER,
  instance_id TEXT,
  question TEXT NOT NULL,
  answer TEXT,
  status TEXT NOT NULL DEFAULT 'open',
  created_at TEXT NOT NULL DEFAULT ({NOW}),
  answered_at TEXT
);

CREATE TABLE IF NOT EXISTS instances (
  id TEXT PRIMARY KEY,
  mode TEXT NOT NULL,
  parent_instance TEXT,
  task TEXT,
  model TEXT,
  depth INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL DEFAULT 'running',
  tokens_used INTEGER NOT NULL DEFAULT 0,
  started_at TEXT NOT NULL DEFAULT ({NOW}),
  ended_at TEXT,
  report TEXT
);

CREATE TABLE IF NOT EXISTS messages (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id TEXT NOT NULL,
  seq INTEGER NOT NULL,
  role TEXT NOT NULL,
  content TEXT NOT NULL DEFAULT '',
  tool_calls TEXT,
  tool_call_id TEXT,
  reasoning TEXT,
  created_at TEXT NOT NULL DEFAULT ({NOW})
);
CREATE INDEX IF NOT EXISTS idx_messages_instance ON messages(instance_id, seq);
CREATE INDEX IF NOT EXISTS idx_messages_role ON messages(role);

CREATE TABLE IF NOT EXISTS tool_calls (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id TEXT NOT NULL,
  message_seq INTEGER NOT NULL DEFAULT 0,
  name TEXT NOT NULL,
  args TEXT NOT NULL DEFAULT '',
  result TEXT NOT NULL DEFAULT '',
  is_error INTEGER NOT NULL DEFAULT 0,
  duration_ms INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT ({NOW})
);
CREATE INDEX IF NOT EXISTS idx_toolcalls_instance ON tool_calls(instance_id);
CREATE INDEX IF NOT EXISTS idx_toolcalls_name ON tool_calls(name);

CREATE TABLE IF NOT EXISTS skills (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  name TEXT UNIQUE NOT NULL,
  path TEXT NOT NULL DEFAULT '',
  description TEXT NOT NULL DEFAULT '',
  content TEXT NOT NULL DEFAULT '',
  uses INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT ({NOW}),
  updated_at TEXT NOT NULL DEFAULT ({NOW})
);

CREATE TABLE IF NOT EXISTS skill_versions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  skill_name TEXT NOT NULL,
  version INTEGER NOT NULL,
  content TEXT NOT NULL,
  description TEXT NOT NULL DEFAULT '',
  author TEXT NOT NULL DEFAULT 'user',
  reason TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT ({NOW})
);

CREATE TABLE IF NOT EXISTS skill_loads (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id TEXT,
  skill_name TEXT NOT NULL,
  created_at TEXT NOT NULL DEFAULT ({NOW})
);
CREATE INDEX IF NOT EXISTS idx_skill_loads_name ON skill_loads(skill_name);

CREATE TABLE IF NOT EXISTS prompt_versions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  mode TEXT NOT NULL,
  version INTEGER NOT NULL,
  content TEXT NOT NULL,
  author TEXT NOT NULL DEFAULT 'user',
  reason TEXT NOT NULL DEFAULT '',
  active INTEGER NOT NULL DEFAULT 0,
  created_at TEXT NOT NULL DEFAULT ({NOW})
);
CREATE INDEX IF NOT EXISTS idx_prompt_mode ON prompt_versions(mode, version);

CREATE TABLE IF NOT EXISTS compactions (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  instance_id TEXT,
  removed_messages INTEGER NOT NULL DEFAULT 0,
  before_tokens INTEGER NOT NULL DEFAULT 0,
  after_tokens INTEGER NOT NULL DEFAULT 0,
  summary TEXT NOT NULL DEFAULT '',
  created_at TEXT NOT NULL DEFAULT ({NOW})
);
"#
        ))?;
        Ok(())
    }

    /// Rename the legacy `sessions`/`session_id` schema to the unified
    /// `instances`/`instance_id` schema. SQLite performs these renames in place,
    /// so recorded history survives the upgrade. A fresh database is untouched.
    fn migrate_session_to_instance(&self) -> Result<()> {
        let has_column = |table: &str, col: &str| -> Result<bool> {
            let mut stmt = self.conn.prepare(&format!("PRAGMA table_info({table})"))?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                if row.get::<_, String>(1)? == col {
                    return Ok(true);
                }
            }
            Ok(false)
        };

        if self.has_table("sessions")? && !self.has_table("instances")? {
            self.conn
                .execute_batch("ALTER TABLE sessions RENAME TO instances;")?;
        }
        for (table, old_col, new_col) in [
            ("instances", "parent_session", "parent_instance"),
            ("messages", "session_id", "instance_id"),
            ("tool_calls", "session_id", "instance_id"),
            ("requirement_questions", "session_id", "instance_id"),
            ("skill_loads", "session_id", "instance_id"),
            ("compactions", "session_id", "instance_id"),
        ] {
            if self.has_table(table)? && has_column(table, old_col)? && !has_column(table, new_col)? {
                self.conn.execute_batch(&format!(
                    "ALTER TABLE {table} RENAME COLUMN {old_col} TO {new_col};"
                ))?;
            }
        }
        // The renamed indexes (SQLite keeps their names when a column is
        // renamed) are superseded by the ones `init_schema` creates.
        self.conn.execute_batch(
            "DROP INDEX IF EXISTS idx_messages_session;\n\
             DROP INDEX IF EXISTS idx_toolcalls_session;",
        )?;
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

    // ---------------------------------------------------------------- tickets

    pub fn ticket_create(
        &self,
        title: &str,
        description: &str,
        priority: i64,
        parent_id: Option<i64>,
        requirement_id: Option<i64>,
        mode: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO tickets(title,description,priority,parent_id,requirement_id,mode,status) VALUES(?,?,?,?,?,?,'open')",
            params![title, description, priority, parent_id, requirement_id, mode],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn ticket_get(&self, id: i64) -> Result<Option<Ticket>> {
        let sql = format!("SELECT {TICKET_COLS} FROM tickets WHERE id=?");
        Ok(self.conn.query_row(&sql, params![id], t_row).optional()?)
    }

    pub fn ticket_list(
        &self,
        status: Option<&str>,
        requirement_id: Option<i64>,
    ) -> Result<Vec<Ticket>> {
        let mut sql = format!("SELECT {TICKET_COLS} FROM tickets WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(s) = status {
            sql.push_str(" AND status=?");
            args.push(Box::new(s.to_string()));
        }
        if let Some(r) = requirement_id {
            sql.push_str(" AND requirement_id=?");
            args.push(Box::new(r));
        }
        sql.push_str(" ORDER BY priority ASC, id ASC");
        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::types::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), t_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Transition a ticket. `status` is open|in_progress|resolved|closed.
    pub fn ticket_set_status(
        &self,
        id: i64,
        status: &str,
        resolution: Option<&str>,
    ) -> Result<bool> {
        let n = self.conn.execute(
            &format!(
                "UPDATE tickets SET status=?, resolution=COALESCE(?,resolution), updated_at={NOW},
                 resolved_at=CASE WHEN ?='resolved' THEN {NOW} ELSE resolved_at END,
                 closed_at=CASE WHEN ?='closed' THEN {NOW} ELSE closed_at END
                 WHERE id=?"
            ),
            params![status, resolution, status, status, id],
        )?;
        Ok(n > 0)
    }

    /// Apply a partial field edit. Returns false when the ticket does not
    /// exist. Status/resolution transitions go through [`Self::ticket_set_status`].
    pub fn ticket_update(&self, id: i64, e: &TicketEdit) -> Result<bool> {
        let mut sets: Vec<&str> = Vec::new();
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(v) = &e.title {
            sets.push("title=?");
            args.push(Box::new(v.clone()));
        }
        if let Some(v) = &e.description {
            sets.push("description=?");
            args.push(Box::new(v.clone()));
        }
        if let Some(v) = e.priority {
            sets.push("priority=?");
            args.push(Box::new(v));
        }
        if let Some(v) = e.parent_id {
            sets.push("parent_id=?");
            args.push(Box::new(v));
        }
        if let Some(v) = e.requirement_id {
            sets.push("requirement_id=?");
            args.push(Box::new(v));
        }
        if sets.is_empty() {
            return Ok(self.ticket_get(id)?.is_some());
        }
        let sql = format!(
            "UPDATE tickets SET {}, updated_at={NOW} WHERE id=?",
            sets.join(", ")
        );
        args.push(Box::new(id));
        let refs: Vec<&dyn rusqlite::types::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let n = self.conn.execute(&sql, refs.as_slice())?;
        Ok(n > 0)
    }

    /// Reopen a resolved/closed ticket, clearing its resolution.
    pub fn ticket_reopen(&self, id: i64) -> Result<bool> {
        let n = self.conn.execute(
            &format!(
                "UPDATE tickets SET status='open', resolution=NULL, resolved_at=NULL, \
                 closed_at=NULL, updated_at={NOW} WHERE id=?"
            ),
            params![id],
        )?;
        Ok(n > 0)
    }

    // ------------------------------------------------------------ requirements
    //
    // Requirements are persisted as markdown files under
    // `.genji/requirements/` (see `reqmd`). The `requirements` table below is
    // legacy: it is retained only so existing workspaces can be migrated into
    // the file store once, on startup.

    /// Whether `table` exists in the database (used for the legacy migration).
    pub fn has_table(&self, table: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            params![table],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// List legacy DB requirements, ordered stakeholder-first. Used only by the
    /// one-time migration to markdown files.
    pub fn requirement_list(
        &self,
        level: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<Requirement>> {
        let mut sql = format!("SELECT {REQ_COLS} FROM requirements WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        if let Some(l) = level {
            sql.push_str(" AND level=?");
            args.push(Box::new(l.to_string()));
        }
        if let Some(s) = status {
            sql.push_str(" AND status=?");
            args.push(Box::new(s.to_string()));
        }
        sql.push_str(" ORDER BY CASE level WHEN 'stakeholder' THEN 0 ELSE 1 END, id ASC");
        let mut stmt = self.conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::types::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), r_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

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

    pub fn question_answer(&self, id: i64, answer: &str) -> Result<()> {
        self.conn.execute(
            &format!("UPDATE requirement_questions SET answer=?,status='answered',answered_at={NOW} WHERE id=?"),
            params![answer, id],
        )?;
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::{Db, TicketEdit};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db(tag: &str) -> (Db, std::path::PathBuf) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("genji-db-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("test.db")).unwrap();
        db.init_schema().unwrap();
        (db, dir)
    }

    #[test]
    fn ticket_update_and_reopen() {
        let (db, dir) = temp_db("tickets");
        let id = db
            .ticket_create("First", "desc", 2, None, Some(3), "plan")
            .unwrap();

        let edit = TicketEdit {
            title: Some("Renamed".into()),
            description: Some("new desc".into()),
            priority: Some(1),
            parent_id: Some(Some(9)),
            requirement_id: Some(None),
        };
        assert!(db.ticket_update(id, &edit).unwrap());
        let t = db.ticket_get(id).unwrap().unwrap();
        assert_eq!(t.title, "Renamed");
        assert_eq!(t.description, "new desc");
        assert_eq!(t.priority, 1);
        assert_eq!(t.parent_id, Some(9));
        assert_eq!(t.requirement_id, None);

        db.ticket_set_status(id, "resolved", Some("done")).unwrap();
        assert_eq!(db.ticket_get(id).unwrap().unwrap().status, "resolved");

        assert!(db.ticket_reopen(id).unwrap());
        let t = db.ticket_get(id).unwrap().unwrap();
        assert_eq!(t.status, "open");
        assert_eq!(t.resolution, None);
        assert!(t.resolved_at.is_none());

        assert!(!db.ticket_update(9999, &edit).unwrap());
        assert!(!db.ticket_reopen(9999).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
