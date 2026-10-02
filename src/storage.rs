pub mod context {
    use anyhow::Result;
    use serde_json::{Value, json};

    use crate::llm::{self, ChatMessage, LlmClient, Role};
    use crate::tools::Tool;

    pub fn estimate_tools(tools: &[Value]) -> i64 {
        let chars = tools
            .iter()
            .map(|tool| tool.to_string().chars().count())
            .fold(0usize, usize::saturating_add);
        llm::estimate_chars(chars)
    }

    #[derive(Debug, Clone, Copy, Default)]
    pub struct ContextInfo {
        pub system_prompt_tokens: i64,
        pub system_tools_tokens: i64,
        pub turn_messages_tokens: i64,
        pub total_tokens: i64,
        pub context_window: i64,
    }

    impl ContextInfo {
        pub fn compute(messages: &[ChatMessage], tools: &[Value], context_window: i64) -> Self {
            let system_prompt_tokens = messages
                .first()
                .map_or(0, super::super::llm::ChatMessage::est_tokens);
            let turn_messages_tokens = messages
                .iter()
                .skip(1)
                .map(super::super::llm::ChatMessage::est_tokens)
                .sum();
            let system_tools_tokens = estimate_tools(tools);
            Self {
                system_prompt_tokens,
                system_tools_tokens,
                turn_messages_tokens,
                total_tokens: system_prompt_tokens + system_tools_tokens + turn_messages_tokens,
                context_window,
            }
        }

        pub fn from_json(v: &Value) -> Self {
            let get = |k: &str| v.get(k).and_then(serde_json::Value::as_i64).unwrap_or(0);
            Self {
                system_prompt_tokens: get("system_prompt_tokens"),
                system_tools_tokens: get("system_tools_tokens"),
                turn_messages_tokens: get("turn_messages_tokens"),
                total_tokens: get("total_tokens"),
                context_window: get("context_window"),
            }
        }

        pub fn percent(&self) -> f64 {
            percent_of(self.total_tokens, self.context_window)
        }

        pub fn to_json(self) -> Value {
            json!({
                "total_tokens": self.total_tokens,
                "context_window": self.context_window,
                "percent": self.percent(),
                "system_prompt_tokens": self.system_prompt_tokens,
                "system_tools_tokens": self.system_tools_tokens,
                "turn_messages_tokens": self.turn_messages_tokens,
            })
        }

        pub fn summary(&self) -> String {
            let p = |tokens: i64| percent_of(tokens, self.context_window);
            format!(
                "context: {} / {} tokens ({:.1}%)\n  system prompt: {} ({:.1}%)\n  system tools: {} ({:.1}%)\n  turn messages: {} ({:.1}%)",
                self.total_tokens,
                self.context_window,
                self.percent(),
                self.system_prompt_tokens,
                p(self.system_prompt_tokens),
                self.system_tools_tokens,
                p(self.system_tools_tokens),
                self.turn_messages_tokens,
                p(self.turn_messages_tokens),
            )
        }
    }

    #[derive(Debug, Clone)]
    pub struct Compaction {
        pub removed: i64,
        pub before: i64,
        pub after: i64,
        pub summary: String,
        pub prompt_tokens: i64,
        pub completion_tokens: i64,
    }

    pub struct ContextComposer {
        messages: Vec<ChatMessage>,
        tools: Vec<Value>,
        context_window: i64,
        last_prompt_tokens: i64,
    }

    impl ContextComposer {
        pub fn new(system: String, tools: Vec<Tool>, context_window: i64) -> Self {
            Self {
                messages: vec![ChatMessage::system(system)],
                tools: tools.into_iter().map(|t| t.to_json()).collect(),
                context_window,
                last_prompt_tokens: 0,
            }
        }

        pub fn messages(&self) -> &[ChatMessage] {
            &self.messages
        }

        pub fn tools(&self) -> &[Value] {
            &self.tools
        }

        pub fn set_last_prompt_tokens(&mut self, tokens: i64) {
            self.last_prompt_tokens = tokens;
        }

        pub fn push(&mut self, msg: ChatMessage) {
            self.messages.push(msg);
        }

        pub fn set_system(&mut self, system: String) {
            if let Some(first) = self.messages.first_mut() {
                first.role = Role::System;
                first.content = system;
            } else {
                self.messages.push(ChatMessage::system(system));
            }
        }

        #[cfg(any(feature = "formal", test))]
        pub fn switch_mode(&mut self, tools: Vec<Tool>, system: String, context_window: i64) {
            self.tools = tools.into_iter().map(|t| t.to_json()).collect();
            self.context_window = context_window;
            self.set_system(system);
        }

        pub fn stats(&self) -> ContextInfo {
            ContextInfo::compute(&self.messages, self.tools(), self.context_window)
        }

        pub fn snapshot(&self) -> Value {
            json!({
                "context_window": self.context_window,
                "last_prompt_tokens": self.last_prompt_tokens,
                "messages": self.messages.iter().map(super::super::llm::ChatMessage::to_json).collect::<Vec<_>>(),
                "tools": self.tools(),
            })
        }

        pub fn maybe_compact(
            &mut self,
            threshold_fraction: f64,
            keep: usize,
            llm: &LlmClient,
        ) -> Result<Option<Compaction>> {
            let threshold = (self.context_window as f64 * threshold_fraction) as i64;
            let est = if self.last_prompt_tokens > 0 {
                self.last_prompt_tokens
            } else {
                llm::estimate_messages(&self.messages)
            };
            if est >= threshold {
                self.compact(keep, llm)
            } else {
                Ok(None)
            }
        }

        pub fn compact(&mut self, keep: usize, llm: &LlmClient) -> Result<Option<Compaction>> {
            let keep = keep.max(2);
            if self.messages.len() <= keep + 2 {
                return Ok(None);
            }
            let mut split = self.messages.len() - keep;
            while split < self.messages.len() && self.messages[split].role == Role::Tool {
                split += 1;
            }
            if split <= 1 {
                return Ok(None);
            }
            let before = llm::estimate_messages(&self.messages);
            let middle: Vec<ChatMessage> = self.messages[1..split].to_vec();
            let rendered_source = render_messages(&middle);
            let rendered = llm::truncate(&rendered_source, 120_000);
            let summary_req = vec![
                ChatMessage::system(
                    "You compress agent running history. Preserve decisions, key clues, open problems. Be dense and factual.",
                ),
                ChatMessage::user(format!(
                    "Summarize this conversation segment:\n\n{rendered}"
                )),
            ];
            let resp = llm.chat(&summary_req, &[])?;
            let summary = resp.message.content.trim().to_string();

            let system = self.messages[0].clone();
            let recent: Vec<ChatMessage> = self.messages[split..].to_vec();
            let removed = i64::try_from(split - 1).unwrap_or(i64::MAX);
            let mut new_msgs = vec![system];
            new_msgs.push(ChatMessage::user(format!(
                "[compacted summary of earlier conversation]\n{summary}"
            )));
            new_msgs.extend(recent);
            self.messages = new_msgs;
            self.last_prompt_tokens = 0;
            Ok(Some(Compaction {
                removed,
                before,
                after: llm::estimate_messages(&self.messages),
                summary,
                prompt_tokens: resp.prompt_tokens,
                completion_tokens: resp.completion_tokens,
            }))
        }
    }

    fn percent_of(tokens: i64, window: i64) -> f64 {
        if window <= 0 {
            0.0
        } else {
            (tokens as f64) * 100.0 / (window as f64)
        }
    }

    fn render_messages(msgs: &[ChatMessage]) -> String {
        let mut out = String::new();
        for m in msgs {
            out.push_str(&format!("[{}] {}", m.role, m.content));
            if !m.tool_calls.is_empty() {
                let calls: Vec<String> = m
                    .tool_calls
                    .iter()
                    .map(|c| format!("{}({})", c.name, llm::truncate(&c.arguments, 200)))
                    .collect();
                out.push_str(&format!("\n  calls: {}", calls.join(", ")));
            }
            if let Some(id) = &m.tool_call_id {
                out.push_str(&format!(" (tool_call_id={id})"));
            }
            out.push_str("\n\n");
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::{ContextComposer, ContextInfo, estimate_tools};
        use crate::llm::{ChatMessage, Role};
        use crate::tools::Tool;
        use serde_json::json;

        fn tool(name: &'static str) -> Tool {
            crate::tools::test_tool(name, json!({"type": "object"}))
        }

        #[test]
        fn splits_context_into_system_tools_and_turns() {
            let messages = vec![
                ChatMessage::system("s".repeat(400)),
                ChatMessage::user("u".repeat(400)),
                ChatMessage::assistant("a".repeat(400)),
            ];
            let tools = vec![json!({"type": "function", "function": {"name": "read"}})];
            let info = ContextInfo::compute(&messages, &tools, 1000);
            assert!(info.system_prompt_tokens > 0);
            assert!(info.turn_messages_tokens > info.system_prompt_tokens);
            assert_eq!(info.system_tools_tokens, estimate_tools(&tools));
            assert_eq!(
                info.total_tokens,
                info.system_prompt_tokens + info.system_tools_tokens + info.turn_messages_tokens
            );
            assert!(info.percent() > 0.0);
        }

        #[test]
        fn json_round_trips() {
            let info = ContextInfo {
                system_prompt_tokens: 10,
                system_tools_tokens: 20,
                turn_messages_tokens: 30,
                total_tokens: 60,
                context_window: 1000,
            };
            let back = ContextInfo::from_json(&info.to_json());
            assert_eq!(back.total_tokens, 60);
            assert_eq!(back.system_tools_tokens, 20);
            assert_eq!(back.turn_messages_tokens, 30);
        }

        #[test]
        fn summary_reports_zero_for_unknown_window() {
            let info = ContextInfo {
                total_tokens: 5,
                context_window: 0,
                ..Default::default()
            };
            assert_eq!(info.percent(), 0.0);
            assert!(info.summary().contains("5 / 0 tokens"));
        }

        #[test]
        fn stats_measure_the_current_conversation() {
            let mut ctx = ContextComposer::new("system".to_string(), vec![tool("read")], 1000);
            let base = ctx.stats();
            assert!(base.system_prompt_tokens > 0);
            assert!(base.system_tools_tokens > 0);
            assert_eq!(base.turn_messages_tokens, 0);

            ctx.push(ChatMessage::user("hello"));
            ctx.push(ChatMessage::tool_result("call_1", "result"));
            assert_eq!(ctx.messages().len(), 3);
            assert_eq!(ctx.messages()[2].role, Role::Tool);

            let stats = ctx.stats();
            assert!(stats.turn_messages_tokens > 0);
            assert_eq!(stats.context_window, 1000);
            assert_eq!(
                stats.total_tokens,
                stats.system_prompt_tokens + stats.system_tools_tokens + stats.turn_messages_tokens
            );
        }

        #[test]
        fn snapshot_returns_live_prompt_without_stats() {
            let mut ctx =
                ContextComposer::new("you are genji".to_string(), vec![tool("read")], 1000);
            ctx.push(ChatMessage::user("hello"));
            ctx.set_last_prompt_tokens(42);
            let snap = ctx.snapshot();
            assert_eq!(snap["context_window"].as_i64(), Some(1000));
            assert_eq!(snap["last_prompt_tokens"].as_i64(), Some(42));
            assert_eq!(snap["messages"][0]["role"], "system");
            assert_eq!(snap["messages"][0]["content"], "you are genji");
            assert_eq!(snap["messages"][1]["role"], "user");
            assert_eq!(snap["tools"][0]["function"]["name"], "read");
            assert!(snap.get("total_tokens").is_none());
        }

        #[test]
        fn switch_mode_replaces_system_and_tools() {
            let mut ctx = ContextComposer::new("old".to_string(), vec![tool("read")], 1000);
            ctx.push(ChatMessage::user("keep me"));
            let turns = ctx.stats().turn_messages_tokens;
            ctx.switch_mode(vec![tool("bash")], "new".to_string(), 2000);
            assert_eq!(ctx.messages()[0].content, "new");
            assert_eq!(ctx.messages()[1].content, "keep me");
            assert_eq!(ctx.tools()[0]["function"]["name"], "bash");
            let stats = ctx.stats();
            assert_eq!(stats.context_window, 2000);
            assert_eq!(stats.turn_messages_tokens, turns);
        }
    }
}
pub mod db {
    use anyhow::{Context, Result};
    use rusqlite::{Connection, OptionalExtension, params};
    use std::path::Path;

    const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

    #[cfg(feature = "formal")]
    #[derive(Debug, Clone, Default)]
    pub enum FieldPatch<T> {
        #[default]
        Keep,
        Set(T),
        Clear,
    }

    #[cfg(feature = "formal")]
    impl<T: Clone> FieldPatch<T> {
        pub fn apply_to(&self, slot: &mut Option<T>) {
            match self {
                Self::Keep => {}
                Self::Set(value) => *slot = Some(value.clone()),
                Self::Clear => *slot = None,
            }
        }
    }

    #[cfg(feature = "formal")]
    #[derive(Default)]
    pub struct TicketEdit {
        pub title: Option<String>,
        pub description: Option<String>,
        pub priority: Option<i64>,
        pub parent_id: FieldPatch<i64>,
        pub requirement_id: FieldPatch<i64>,
    }

    #[derive(Debug, Clone)]
    pub struct SkillRow {
        pub id: i64,
        pub name: String,
        pub path: String,
        pub description: String,
        pub content: String,
        pub uses: i64,
    }

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
            params![id, mode, parent, task, model, i64::from(depth)],
        )?;
            Ok(())
        }

        pub fn instance_end(
            &self,
            id: &str,
            status: &str,
            tokens: i64,
            report: &str,
        ) -> Result<()> {
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
            params![instance_id, message_seq, name, args, result, i64::from(is_error), duration_ms],
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

        pub fn has_skills(&self) -> Result<bool> {
            Ok(self
                .conn
                .query_row("SELECT EXISTS(SELECT 1 FROM skills)", [], |row| row.get(0))?)
        }

        pub fn skill_list(&self) -> Result<Vec<SkillRow>> {
            let mut stmt = self.conn.prepare(
                "SELECT id,name,path,description,content,uses FROM skills ORDER BY name",
            )?;
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

        pub fn skill_version_get(
            &self,
            name: &str,
            version: i64,
        ) -> Result<Option<(String, String)>> {
            Ok(self
            .conn
            .query_row(
                "SELECT content,description FROM skill_versions WHERE skill_name=? AND version=?",
                params![name, version],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        }

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
}
pub mod events {
    //! Machine-readable event stream.

    use serde_json::{Value, json};
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    pub struct EventEmitter {
        instance: String,
        seq: AtomicU64,
        out: Mutex<Box<dyn Write + Send>>,
        trace: Mutex<Option<std::fs::File>>,
    }

    impl EventEmitter {
        pub fn new(instance: impl Into<String>, trace_path: Option<PathBuf>) -> io::Result<Self> {
            Self::with_writer_and_trace(instance, Box::new(io::stdout()), trace_path)
        }

        #[cfg(test)]
        pub fn with_writer(instance: impl Into<String>, out: Box<dyn Write + Send>) -> Self {
            Self::with_writer_and_trace(instance, out, None)
                .expect("an emitter without a trace file cannot fail to initialize")
        }

        fn with_writer_and_trace(
            instance: impl Into<String>,
            out: Box<dyn Write + Send>,
            trace_path: Option<PathBuf>,
        ) -> io::Result<Self> {
            let trace = match trace_path {
                Some(p) => {
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    Some(
                        std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&p)?,
                    )
                }
                None => None,
            };
            Ok(Self {
                instance: instance.into(),
                seq: AtomicU64::new(0),
                out: Mutex::new(out),
                trace: Mutex::new(trace),
            })
        }

        fn now_ms() -> u64 {
            crate::storage::util::unix_millis()
        }

        pub fn emit(&self, mut event: Value) {
            if let Some(obj) = event.as_object_mut() {
                let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
                obj.insert("seq".into(), json!(seq));
                obj.insert("ts".into(), json!(Self::now_ms()));
                if !self.instance.is_empty() && !obj.contains_key("instance") {
                    obj.insert("instance".into(), json!(self.instance));
                }
            }
            let Ok(line) = serde_json::to_string(&event) else {
                return;
            };
            if let Ok(mut trace) = self.trace.lock()
                && let Some(f) = trace.as_mut()
                && writeln!(f, "{line}").and_then(|()| f.flush()).is_err()
            {
                eprintln!("[events] failed to append to the event file");
            }
            if let Ok(mut out) = self.out.lock() {
                let _ = writeln!(out, "{line}");
                let _ = out.flush();
            }
        }

        pub fn instance_start(
            &self,
            workspace: &str,
            mode: &str,
            model: &str,
            parent: Option<&str>,
            depth: u32,
            task: &str,
        ) {
            self.emit(json!({
                "type": "instance_start",
                "workspace": workspace,
                "mode": mode,
                "model": model,
                "parent": parent,
                "depth": depth,
                "task": task,
            }));
        }

        pub fn user(&self, content: &str) {
            self.emit(json!({ "type": "user", "content": content }));
        }

        pub fn assistant(&self, content: &str, reasoning: Option<&str>) {
            self.emit(json!({
                "type": "assistant",
                "content": content,
                "reasoning": reasoning,
            }));
        }

        pub fn tool_call(&self, id: &str, name: &str, arguments: &str) {
            let mut event = json!({ "type": "tool_call", "id": id, "name": name });
            match serde_json::from_str::<Value>(arguments) {
                Ok(args) => event["arguments"] = args,
                Err(_) => event["raw_arguments"] = json!(arguments),
            }
            self.emit(event);
        }

        pub fn tool_result(
            &self,
            id: &str,
            name: &str,
            is_error: bool,
            duration_ms: i64,
            result: &str,
        ) {
            self.emit(json!({
                "type": "tool_result",
                "id": id,
                "name": name,
                "is_error": is_error,
                "duration_ms": duration_ms,
                "result": result,
            }));
        }

        pub fn tokens(&self, used: i64, prompt: i64, completion: i64) {
            self.emit(json!({
                "type": "tokens",
                "used": used,
                "prompt": prompt,
                "completion": completion,
            }));
        }

        pub fn status(&self, status: &str) {
            self.emit(json!({ "type": "status", "status": status }));
        }

        #[cfg(feature = "formal")]
        pub fn mode(&self, mode: &str, model: &str) {
            self.emit(json!({ "type": "mode", "mode": mode, "model": model }));
        }

        pub fn compaction(&self, removed: i64, before: i64, after: i64, summary: &str) {
            self.emit(json!({
                "type": "compaction",
                "removed": removed,
                "before": before,
                "after": after,
                "summary": summary,
            }));
        }

        #[cfg(feature = "formal")]
        pub fn cycle(&self, cycle: usize, max: usize, mode: &str, active_requirements: i64) {
            self.emit(json!({
                "type": "cycle",
                "cycle": cycle,
                "max": max,
                "mode": mode,
                "active_requirements": active_requirements,
            }));
        }

        pub fn error(&self, message: &str) {
            self.emit(json!({ "type": "error", "message": message }));
        }

        pub fn instance_end(&self, status: &str, tokens_used: i64, report: &str) {
            self.emit(json!({
                "type": "instance_end",
                "status": status,
                "tokens_used": tokens_used,
                "report": report,
            }));
        }
    }

    #[cfg(test)]
    mod tests {
        use super::EventEmitter;
        use serde_json::Value;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        fn emitter() -> (EventEmitter, SharedBuf) {
            let buf = SharedBuf::default();
            let e = EventEmitter::with_writer("sess-1", Box::new(buf.clone()));
            (e, buf)
        }

        fn lines(buf: &SharedBuf) -> Vec<Value> {
            let data = buf.0.lock().unwrap().clone();
            String::from_utf8(data)
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect()
        }

        #[test]
        fn events_are_jsonl_with_seq_ts_and_instance() {
            let (e, buf) = emitter();
            e.user("hello");
            e.tool_call("call_1", "read", "{\"path\":\"a.txt\"}");
            let out = lines(&buf);
            assert_eq!(out.len(), 2);
            assert_eq!(out[0]["type"], "user");
            assert_eq!(out[0]["seq"], 1);
            assert_eq!(out[0]["instance"], "sess-1");
            assert!(out[0]["ts"].is_u64());
            assert_eq!(out[1]["type"], "tool_call");
            assert_eq!(out[1]["arguments"]["path"], "a.txt");
            assert!(out[1].get("raw_arguments").is_none());
        }

        #[test]
        fn invalid_tool_arguments_are_preserved_raw() {
            let (e, buf) = emitter();
            e.tool_call("id", "bash", "not json");
            let out = lines(&buf);
            assert!(out[0].get("arguments").is_none());
            assert_eq!(out[0]["raw_arguments"], "not json");
        }

        #[test]
        fn event_file_creation_is_fail_fast() {
            let parent = std::env::temp_dir().join(format!(
                "genji-events-blocked-{}-{}",
                std::process::id(),
                super::EventEmitter::now_ms()
            ));
            let _ = std::fs::remove_file(&parent);
            std::fs::write(&parent, "not a directory").unwrap();
            let result = EventEmitter::new("sess-fail", Some(parent.join("events.jsonl")));
            assert!(result.is_err());
            let _ = std::fs::remove_file(&parent);
        }

        #[test]
        fn events_are_appended_to_the_trace_file() {
            let path = std::env::temp_dir().join(format!(
                "genji-events-test-{}-{}.jsonl",
                std::process::id(),
                super::EventEmitter::now_ms()
            ));
            let _ = std::fs::remove_file(&path);
            let e = EventEmitter::new("sess-trace", Some(path.clone())).unwrap();
            e.user("one");
            e.instance_end("done", 3, "report text");
            let text = std::fs::read_to_string(&path).unwrap();
            let parsed: Vec<Value> = text
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            assert_eq!(parsed[0]["type"], "user");
            assert_eq!(parsed[1]["type"], "instance_end");
            assert_eq!(parsed[1]["report"], "report text");
            let _ = std::fs::remove_file(&path);
        }
    }
}
pub mod modes {
    use anyhow::{Result, bail};
    use clap::ValueEnum;
    use serde::{Deserialize, Serialize};
    use std::fmt;
    use std::str::FromStr;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
    #[serde(rename_all = "lowercase")]
    pub enum Mode {
        Plan,
        Build,
        Explore,
        Retro,
    }

    impl Mode {
        pub const fn as_str(self) -> &'static str {
            match self {
                Mode::Plan => "plan",
                Mode::Build => "build",
                Mode::Explore => "explore",
                Mode::Retro => "retro",
            }
        }

        pub fn parse(s: &str) -> Result<Mode> {
            match s.to_ascii_lowercase().as_str() {
                "plan" => Ok(Mode::Plan),
                "build" => Ok(Mode::Build),
                "explore" => Ok(Mode::Explore),
                "retro" => Ok(Mode::Retro),
                other => bail!("unknown mode `{other}` (expected plan|build|explore|retro)"),
            }
        }

        pub const fn core_prompt(self) -> &'static str {
            match self {
                Mode::Plan => include_str!("prompts/plan.md"),
                Mode::Build => include_str!("prompts/build.md"),
                Mode::Explore => include_str!("prompts/explore.md"),
                Mode::Retro => include_str!("prompts/retro.md"),
            }
        }

        pub const fn allows_extended(self) -> bool {
            !matches!(self, Mode::Retro)
        }

        pub const fn formal_guidance(self) -> &'static str {
            match self {
                Mode::Plan => include_str!("prompts/formal-plan.md"),
                Mode::Build => include_str!("prompts/formal-build.md"),
                _ => "",
            }
        }

        pub fn all() -> [Mode; 4] {
            [Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro]
        }
    }

    impl fmt::Display for Mode {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.as_str())
        }
    }

    impl FromStr for Mode {
        type Err = anyhow::Error;

        fn from_str(s: &str) -> Result<Self> {
            Self::parse(s)
        }
    }

    pub fn shared_preamble() -> &'static str {
        include_str!("prompts/shared.md")
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn retro_has_no_extended_prompt() {
            assert!(!Mode::Retro.allows_extended());
        }

        #[test]
        fn other_modes_are_extensible() {
            for mode in [Mode::Plan, Mode::Build, Mode::Explore] {
                assert!(mode.allows_extended());
            }
        }
    }
}
pub mod proc {
    use anyhow::{Context, Result};
    use std::fs::File;
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Debug, Clone)]
    pub struct ProcResult {
        pub code: Option<i32>,
        pub stdout: String,
        pub stderr: String,
        pub timed_out: bool,
        pub duration_ms: u128,
    }

    fn tmp_path(dir: &Path, tag: &str) -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!(".{tag}-{}-{n}.tmp", std::process::id()))
    }

    fn read_capped(path: &Path, cap: usize) -> String {
        let Ok(f) = File::open(path) else {
            return String::new();
        };
        let mut buf = Vec::new();
        let _ = f.take(cap as u64).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).to_string()
    }

    pub fn run_capture(
        program: &str,
        args: &[String],
        cwd: &Path,
        tmpdir: &Path,
        timeout: Duration,
        max_read_bytes: usize,
    ) -> Result<ProcResult> {
        std::fs::create_dir_all(tmpdir).ok();
        let out_path = tmp_path(tmpdir, "out");
        let err_path = tmp_path(tmpdir, "err");
        let out_file = File::create(&out_path).context("creating stdout temp")?;
        let err_file = File::create(&err_path).context("creating stderr temp")?;

        let start = Instant::now();
        let mut child = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_file))
            .stderr(Stdio::from(err_file))
            .spawn()
            .with_context(|| format!("spawning `{program}`"))?;

        let mut timed_out = false;
        let poll = Duration::from_millis(25);
        let code = loop {
            if let Some(status) = child.try_wait()? {
                break status.code();
            }
            if start.elapsed() >= timeout {
                let _ = child.kill();
                let _ = child.wait();
                timed_out = true;
                break None;
            }
            std::thread::sleep(poll);
        };

        let stdout = read_capped(&out_path, max_read_bytes);
        let stderr = read_capped(&err_path, max_read_bytes);
        let _ = std::fs::remove_file(&out_path);
        let _ = std::fs::remove_file(&err_path);

        Ok(ProcResult {
            code,
            stdout,
            stderr,
            timed_out,
            duration_ms: start.elapsed().as_millis(),
        })
    }

    pub fn run_bash(
        command: &str,
        cwd: &Path,
        tmpdir: &Path,
        timeout: Duration,
        max_read_bytes: usize,
    ) -> Result<ProcResult> {
        run_capture(
            "bash",
            &["-c".to_string(), command.to_string()],
            cwd,
            tmpdir,
            timeout,
            max_read_bytes,
        )
    }
}
pub mod registry {
    //! A lightweight registry of running genji instances.

    use anyhow::{Context, Result, bail};
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Instance {
        pub id: String,
        pub pid: u32,
        pub workspace: String,
        pub control_socket: String,
        #[serde(default)]
        pub label: String,
        pub started_at: u64,
    }

    impl Instance {
        pub fn path(&self) -> PathBuf {
            dir().join(format!("{}.json", self.id))
        }

        pub fn save(&self) -> Result<()> {
            let d = dir();
            std::fs::create_dir_all(&d)
                .with_context(|| format!("creating instance registry {}", d.display()))?;
            let p = self.path();
            let text = serde_json::to_string_pretty(self)?;
            std::fs::write(&p, format!("{text}\n"))
                .with_context(|| format!("writing instance record {}", p.display()))?;
            Ok(())
        }

        pub fn uptime_secs(&self) -> u64 {
            now_secs().saturating_sub(self.started_at)
        }

        pub fn is_live(&self) -> bool {
            crate::socket::send(Path::new(&self.control_socket), "/ping")
                .is_ok_and(|r| !r.trim().is_empty())
        }
    }

    pub fn dir() -> PathBuf {
        if let Ok(d) = std::env::var("GENJI_REGISTRY_DIR")
            && !d.trim().is_empty()
        {
            return PathBuf::from(d);
        }
        if let Ok(home) = std::env::var("HOME")
            && !home.trim().is_empty()
        {
            return PathBuf::from(home).join(".genji").join("instances");
        }
        std::env::temp_dir().join("genji-instances")
    }

    pub fn events_dir() -> PathBuf {
        dir().join("events")
    }

    pub fn events_path(id: &str) -> PathBuf {
        events_dir().join(format!("{id}.jsonl"))
    }

    pub fn now_secs() -> u64 {
        crate::storage::util::unix_secs()
    }

    fn candidate(seed: u64) -> u64 {
        let mut x = seed;
        if x == 0 {
            x = 0x9e37_79b9_7f4a_7c15;
        }
        // mix so small pid/time deltas spread across the id space
        x ^= x >> 33;
        x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
        x ^= x >> 33;
        x
    }

    /// A short, human-friendly instance id, unique among currently-registered ids.
    pub fn new_id() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        let mut x = candidate(nanos ^ (u64::from(std::process::id()) << 21));
        let d = dir();
        for _ in 0..64 {
            let id = format!("{:06x}", x & 0x00ff_ffff);
            if !d.join(format!("{id}.json")).exists() {
                return id;
            }
            x = candidate(x);
        }
        format!(
            "{:08x}",
            (nanos ^ u64::from(std::process::id())) & u64::from(u32::MAX)
        )
    }

    pub fn load_all() -> Vec<Instance> {
        let d = dir();
        let mut out = Vec::new();
        let Ok(rd) = std::fs::read_dir(&d) else {
            return out;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&p) else {
                continue;
            };
            match serde_json::from_str::<Instance>(&text) {
                Ok(inst) => out.push(inst),
                Err(_) => {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        out.sort_by_key(|i| (i.started_at, i.id.clone()));
        out
    }

    pub fn list_live() -> Vec<Instance> {
        let mut out = Vec::new();
        for inst in load_all() {
            if inst.is_live() {
                out.push(inst);
            } else {
                remove(&inst.id);
            }
        }
        out
    }

    pub fn find(id: &str) -> Result<Instance> {
        let id = id.trim();
        let live = list_live();

        if let Some(inst) = live.iter().find(|i| i.id == id) {
            return Ok(inst.clone());
        }

        if id.is_empty() {
            bail!("missing instance id (see `genji list`)");
        }

        let matches: Vec<&Instance> = live.iter().filter(|i| i.id.starts_with(id)).collect();
        match matches.as_slice() {
            [inst] => Ok((*inst).clone()),
            [] => bail!("no running genji instance with id `{id}` (see `genji list`)"),
            many => {
                let ids: Vec<&str> = many.iter().map(|i| i.id.as_str()).collect();
                bail!(
                    "instance id `{id}` is ambiguous; matches: {} (use a longer prefix)",
                    ids.join(", ")
                )
            }
        }
    }

    pub fn remove(id: &str) {
        let _ = std::fs::remove_file(dir().join(format!("{id}.json")));
    }
}
pub mod util {
    //! Small shared filesystem helpers.

    #[cfg(feature = "formal")]
    use anyhow::{Context, Result};
    #[cfg(feature = "formal")]
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    pub fn expand_home(path: &str) -> PathBuf {
        path.strip_prefix("~/").map_or_else(
            || PathBuf::from(path),
            |rest| {
                std::env::var_os("HOME").map_or_else(
                    || PathBuf::from(path),
                    |home| PathBuf::from(home).join(rest),
                )
            },
        )
    }

    pub fn unix_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_secs())
    }

    pub fn unix_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            })
    }

    /// Expand a leading `~/`, then resolve relative paths against `workspace`.
    pub fn resolve_path(workspace: &Path, path: &str) -> PathBuf {
        let expanded = expand_home(path);
        if expanded.is_absolute() {
            expanded
        } else {
            workspace.join(expanded)
        }
    }

    pub fn relative_path(workspace: &Path, path: &Path) -> String {
        path.strip_prefix(workspace)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned()
    }

    #[cfg(feature = "formal")]
    pub struct MarkdownDocument {
        pub meta: BTreeMap<String, String>,
        pub heading: Option<String>,
        pub body: String,
        pub path: PathBuf,
    }

    #[cfg(feature = "formal")]
    pub fn parse_markdown(path: PathBuf, text: &str) -> MarkdownDocument {
        let (meta, markdown) = split_frontmatter(text);
        let (heading, body) = split_heading(&markdown);
        MarkdownDocument {
            meta,
            heading,
            body,
            path,
        }
    }

    #[cfg(feature = "formal")]
    pub fn nonempty_meta(meta: &BTreeMap<String, String>, key: &str) -> Option<String> {
        meta.get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }

    #[cfg(feature = "formal")]
    pub fn meta_i64(meta: &BTreeMap<String, String>, key: &str) -> Option<i64> {
        meta.get(key).and_then(|v| v.parse::<i64>().ok())
    }

    #[cfg(feature = "formal")]
    pub fn assign_missing_ids(raws: &mut [MarkdownDocument]) -> Vec<bool> {
        let mut next_id = raws
            .iter()
            .filter_map(|raw| meta_i64(&raw.meta, "id"))
            .max()
            .unwrap_or(0)
            + 1;
        raws.iter_mut()
            .map(|raw| {
                if meta_i64(&raw.meta, "id").is_some() {
                    false
                } else {
                    raw.meta.insert("id".into(), next_id.to_string());
                    next_id += 1;
                    true
                }
            })
            .collect()
    }

    #[cfg(feature = "formal")]
    pub fn write_markdown(path: &Path, text: &str) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    #[cfg(feature = "formal")]
    pub fn replace_backing_file(old_path: &Path, new_path: &Path, text: &str) -> Result<()> {
        write_markdown(new_path, text)?;
        if new_path != old_path && old_path.exists() {
            let _ = std::fs::remove_file(old_path);
        }
        Ok(())
    }

    #[cfg(feature = "formal")]
    pub fn load_markdown_dir(root: &Path) -> Result<Vec<MarkdownDocument>> {
        fn walk(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
            if !dir.exists() {
                return Ok(());
            }
            for entry in
                std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?
            {
                let path = entry?.path();
                if path.is_dir() {
                    walk(&path, files)?;
                } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
                    files.push(path);
                }
            }
            Ok(())
        }

        let mut files = Vec::new();
        walk(root, &mut files)?;
        files.sort();
        files
            .into_iter()
            .filter_map(|path| match std::fs::read_to_string(&path) {
                Ok(text) if text.trim().is_empty() => None,
                result => Some((path, result)),
            })
            .map(|(path, text)| {
                let text = text.with_context(|| format!("reading {}", path.display()))?;
                Ok(parse_markdown(path, &text))
            })
            .collect()
    }

    #[cfg(feature = "formal")]
    fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
        let mut meta = BTreeMap::new();
        if let Some(rest) = text.strip_prefix("---\n")
            && let Some(index) = rest.find("\n---")
        {
            for line in rest[..index].lines() {
                if let Some((key, value)) = line.split_once(':') {
                    meta.insert(
                        key.trim().to_owned(),
                        value.trim().trim_matches('"').to_owned(),
                    );
                }
            }
            let body = &rest[index + 4..];
            return (meta, body.strip_prefix('\n').unwrap_or(body).to_owned());
        }
        (meta, text.to_owned())
    }

    #[cfg(feature = "formal")]
    fn split_heading(text: &str) -> (Option<String>, String) {
        let mut heading = None;
        let body = text
            .lines()
            .filter(|line| {
                if heading.is_none()
                    && let Some(value) = line.strip_prefix("# ")
                {
                    heading = Some(value.trim().to_owned());
                    false
                } else {
                    true
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_owned();
        (heading, body)
    }

    pub fn slugify(title: &str, fallback: &str) -> String {
        let mut out = String::new();
        let mut prev_dash = false;
        for c in title.chars() {
            if c.is_ascii_alphanumeric() {
                out.push(c.to_ascii_lowercase());
                prev_dash = false;
            } else if !prev_dash {
                out.push('-');
                prev_dash = true;
            }
            if out.len() >= 60 {
                break;
            }
        }
        let s = out.trim_matches('-').to_string();
        if s.is_empty() {
            fallback.to_string()
        } else {
            s
        }
    }

    #[cfg(test)]
    mod tests {
        use super::{resolve_path, slugify};
        use std::path::Path;

        #[test]
        fn resolves_workspace_and_home_paths() {
            let workspace = Path::new("/workspace");
            assert_eq!(
                resolve_path(workspace, "src/main.rs"),
                workspace.join("src/main.rs")
            );
            assert_eq!(resolve_path(workspace, "/tmp/file"), Path::new("/tmp/file"));
        }

        #[test]
        fn slugifies() {
            assert_eq!(
                slugify("Accept image files & URLs!", "x"),
                "accept-image-files-urls"
            );
            assert_eq!(slugify("Hello__World", "x"), "hello-world");
            assert_eq!(slugify("", "plan"), "plan");
            assert_eq!(slugify("!!!", "plan"), "plan");
        }
    }
}
#[cfg(feature = "formal")]
pub mod reqmd {
    //! File-backed requirement store.

    use anyhow::{Context, Result};
    use serde::{Deserialize, Serialize};
    use std::fmt;
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use crate::config::Config;
    use crate::storage::db::FieldPatch;
    use crate::storage::util::{
        assign_missing_ids, load_markdown_dir, meta_i64, nonempty_meta, relative_path,
        replace_backing_file, slugify, write_markdown,
    };

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum RequirementLevel {
        #[default]
        Stakeholder,
        System,
    }

    impl RequirementLevel {
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::Stakeholder => "stakeholder",
                Self::System => "system",
            }
        }
    }

    impl fmt::Display for RequirementLevel {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.as_str())
        }
    }

    impl FromStr for RequirementLevel {
        type Err = &'static str;

        fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
            match s.trim().trim_matches('"').to_ascii_lowercase().as_str() {
                "stakeholder" => Ok(Self::Stakeholder),
                "system" => Ok(Self::System),
                _ => Err("level must be stakeholder|system"),
            }
        }
    }

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "lowercase")]
    pub enum RequirementStatus {
        #[default]
        Active,
        Met,
        Removed,
    }

    impl RequirementStatus {
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::Active => "active",
                Self::Met => "met",
                Self::Removed => "removed",
            }
        }
    }

    impl fmt::Display for RequirementStatus {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.as_str())
        }
    }

    impl FromStr for RequirementStatus {
        type Err = &'static str;

        fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
            match s.trim().trim_matches('"').to_ascii_lowercase().as_str() {
                "active" => Ok(Self::Active),
                "met" => Ok(Self::Met),
                "removed" => Ok(Self::Removed),
                _ => Err("status must be active|met|removed"),
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct Requirement {
        pub id: i64,
        pub level: RequirementLevel,
        pub title: String,
        pub body: String,
        pub status: RequirementStatus,
        pub parent_id: Option<i64>,
        pub source: String,
        pub path: PathBuf,
        pub created_at: String,
        pub updated_at: String,
    }

    impl Requirement {
        pub fn display_path(&self, workspace: &Path) -> String {
            relative_path(workspace, &self.path)
        }
    }

    fn now_secs() -> String {
        crate::storage::util::unix_secs().to_string()
    }

    fn title_for(raw: &crate::storage::util::MarkdownDocument, fallback: &str) -> String {
        nonempty_meta(&raw.meta, "title")
            .or_else(|| raw.heading.clone())
            .unwrap_or_else(|| {
                raw.path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(fallback)
                    .to_string()
            })
    }

    fn render(r: &Requirement) -> String {
        let mut s = String::new();
        s.push_str("---\n");
        let _ = writeln!(s, "id: {}", r.id);
        let _ = writeln!(s, "level: {}", r.level);
        let _ = writeln!(s, "status: {}", r.status);
        if let Some(p) = r.parent_id {
            let _ = writeln!(s, "parent: {p}");
        }
        let _ = writeln!(s, "source: {}", r.source);
        let _ = writeln!(s, "created: {}", r.created_at);
        let _ = writeln!(s, "updated: {}", r.updated_at);
        s.push_str("---\n\n");
        let _ = writeln!(s, "# {}", r.title);
        if !r.body.trim().is_empty() {
            s.push('\n');
            s.push_str(r.body.trim());
            s.push('\n');
        }
        s
    }

    fn write_at(path: &Path, r: &Requirement) -> Result<()> {
        write_markdown(path, &render(r))
    }

    fn path_for(cfg: &Config, workspace: &Path, r: &Requirement) -> PathBuf {
        cfg.requirements_path(workspace).join(format!(
            "{}-{}.md",
            r.id,
            slugify(&r.title, "requirement")
        ))
    }

    pub fn load_all(cfg: &Config, workspace: &Path) -> Result<Vec<Requirement>> {
        let mut raws = load_markdown_dir(&cfg.requirements_path(workspace))?;
        let assigned = assign_missing_ids(&mut raws);

        let mut out = Vec::new();
        for (raw, assigned_id) in raws.into_iter().zip(assigned) {
            let id = meta_i64(&raw.meta, "id").expect("assign_missing_ids must populate ids");
            let level = nonempty_meta(&raw.meta, "level")
                .and_then(|v| v.parse().ok())
                .unwrap_or_default();
            let title = title_for(&raw, "requirement");
            let status = nonempty_meta(&raw.meta, "status")
                .and_then(|v| v.parse().ok())
                .unwrap_or_default();
            let parent_id = meta_i64(&raw.meta, "parent");
            let source = nonempty_meta(&raw.meta, "source").unwrap_or_else(|| "user_md".into());
            let created_at = nonempty_meta(&raw.meta, "created").unwrap_or_else(now_secs);
            let updated_at =
                nonempty_meta(&raw.meta, "updated").unwrap_or_else(|| created_at.clone());

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
            if assigned_id {
                write_at(&raw.path, &req)?;
            }
            out.push(req);
        }

        out.sort_by_key(|r| (i32::from(r.level != RequirementLevel::Stakeholder), r.id));
        Ok(out)
    }

    pub fn load_by_id(cfg: &Config, workspace: &Path, id: i64) -> Result<Option<Requirement>> {
        Ok(load_all(cfg, workspace)?.into_iter().find(|r| r.id == id))
    }

    pub fn active_count(cfg: &Config, workspace: &Path) -> Result<i64> {
        Ok(load_all(cfg, workspace)?
            .iter()
            .filter(|r| r.status == RequirementStatus::Active)
            .count() as i64)
    }

    pub fn create(
        cfg: &Config,
        workspace: &Path,
        level: RequirementLevel,
        title: &str,
        body: &str,
        parent_id: Option<i64>,
        source: &str,
    ) -> Result<Requirement> {
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
            status: RequirementStatus::Active,
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
        status: Option<RequirementStatus>,
        level: Option<RequirementLevel>,
        parent_id: FieldPatch<i64>,
    ) -> Result<bool> {
        let mut req = match load_by_id(cfg, workspace, id)? {
            Some(r) => r,
            None => return Ok(false),
        };
        if let Some(level) = level {
            req.level = level;
        }
        if let Some(t) = title {
            req.title = t.to_string();
        }
        if let Some(b) = body {
            req.body = b.to_string();
        }
        if let Some(status) = status {
            req.status = status;
        }
        parent_id.apply_to(&mut req.parent_id);
        req.updated_at = now_secs();

        let old_path = req.path.clone();
        let new_path = path_for(cfg, workspace, &req);
        req.path = new_path.clone();
        replace_backing_file(&old_path, &new_path, &render(&req))?;
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
        update(
            cfg,
            workspace,
            id,
            None,
            None,
            Some(RequirementStatus::Removed),
            None,
            FieldPatch::Keep,
        )
    }

    pub fn sync(cfg: &Config, workspace: &Path) -> Result<usize> {
        Ok(load_all(cfg, workspace)?.len())
    }

    #[cfg(test)]
    mod tests {
        use super::{
            RequirementLevel, RequirementStatus, active_count, create, load_all, load_by_id,
            remove, update,
        };
        use crate::config::Config;
        use crate::storage::db::FieldPatch;
        use crate::storage::util::slugify;
        use std::time::{SystemTime, UNIX_EPOCH};

        #[test]
        fn level_from_frontmatter() {
            let ws = temp_workspace("level");
            let cfg = Config::default();
            let dir = cfg.requirements_path(&ws);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("a.md"), "---\nlevel: system\n---\n# A\n").unwrap();
            std::fs::write(dir.join("b.md"), "# B\n").unwrap();

            let all = load_all(&cfg, &ws).unwrap();
            let a = all.iter().find(|r| r.title == "A").unwrap();
            let b = all.iter().find(|r| r.title == "B").unwrap();
            assert_eq!(a.level, RequirementLevel::System);
            assert_eq!(b.level, RequirementLevel::Stakeholder);

            let _ = std::fs::remove_dir_all(&ws);
        }

        #[test]
        fn heading_extraction() {
            let titled = crate::storage::util::parse_markdown(
                std::path::PathBuf::from("test.md"),
                "intro\n# Real Title\nbody",
            );
            assert_eq!(titled.heading, Some("Real Title".into()));
            assert_eq!(titled.body, "intro\nbody");
            let plain = crate::storage::util::parse_markdown(
                std::path::PathBuf::from("test.md"),
                "no heading",
            );
            assert_eq!(plain.heading, None);
            assert_eq!(plain.body, "no heading");
        }

        #[test]
        fn frontmatter_parsing() {
            let doc = crate::storage::util::parse_markdown(
                std::path::PathBuf::from("test.md"),
                "---\nid: 4\nlevel: system\n---\n# T\nbody\n",
            );
            assert_eq!(doc.meta.get("id").map(String::as_str), Some("4"));
            assert_eq!(doc.meta.get("level").map(String::as_str), Some("system"));
            assert_eq!(doc.heading, Some("T".into()));
            assert_eq!(doc.body, "body");
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
                .map_or(0, |d| d.as_nanos());
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
                RequirementLevel::Stakeholder,
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
                RequirementLevel::System,
                "Accept URLs",
                "Accept image URLs.",
                Some(1),
                "agent",
            )
            .unwrap();
            assert_eq!(r2.id, 2);
            assert_eq!(active_count(&cfg, &ws).unwrap(), 2);
            assert!(
                update(
                    &cfg,
                    &ws,
                    2,
                    Some("Accept Files and URLs"),
                    None,
                    Some(RequirementStatus::Met),
                    None,
                    FieldPatch::Keep
                )
                .unwrap()
            );
            let r2b = load_by_id(&cfg, &ws, 2).unwrap().unwrap();
            assert_eq!(r2b.title, "Accept Files and URLs");
            assert_eq!(r2b.status, RequirementStatus::Met);
            assert_eq!(r2b.parent_id, Some(1));
            assert!(
                r2b.display_path(&ws)
                    .ends_with("2-accept-files-and-urls.md")
            );
            assert_eq!(active_count(&cfg, &ws).unwrap(), 1);

            update(
                &cfg,
                &ws,
                2,
                None,
                None,
                None,
                Some(RequirementLevel::Stakeholder),
                FieldPatch::Keep,
            )
            .unwrap();
            let r2c = load_by_id(&cfg, &ws, 2).unwrap().unwrap();
            assert_eq!(r2c.level, RequirementLevel::Stakeholder);
            assert!(
                r2c.display_path(&ws)
                    .ends_with(".genji/requirements/2-accept-files-and-urls.md")
            );

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
            let dir = cfg.requirements_path(&ws);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("hand-written.md"), "# Hand Written\nBody.\n").unwrap();

            let all = load_all(&cfg, &ws).unwrap();
            assert_eq!(all.len(), 1);
            assert_eq!(all[0].id, 1);
            assert_eq!(all[0].level, RequirementLevel::Stakeholder);
            let text = std::fs::read_to_string(dir.join("hand-written.md")).unwrap();
            assert!(
                text.contains("id: 1"),
                "frontmatter should gain an id:\n{text}"
            );

            let _ = std::fs::remove_dir_all(&ws);
        }
    }
}
#[cfg(feature = "formal")]
pub mod ticketmd {
    //! File-backed store for formal-mode tickets.

    use anyhow::Result;
    use serde::{Deserialize, Serialize};
    use std::fmt;
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};
    use std::str::FromStr;

    use crate::config::Config;
    use crate::storage::db::TicketEdit;
    use crate::storage::util::{
        assign_missing_ids, load_markdown_dir, meta_i64, nonempty_meta, relative_path,
        replace_backing_file, slugify, write_markdown,
    };

    #[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum TicketStatus {
        #[default]
        Open,
        InProgress,
        Resolved,
        Closed,
    }

    impl TicketStatus {
        pub const fn as_str(self) -> &'static str {
            match self {
                Self::Open => "open",
                Self::InProgress => "in_progress",
                Self::Resolved => "resolved",
                Self::Closed => "closed",
            }
        }

        pub const fn is_open(self) -> bool {
            matches!(self, Self::Open | Self::InProgress)
        }

        pub const fn is_done(self) -> bool {
            matches!(self, Self::Resolved | Self::Closed)
        }
    }

    impl fmt::Display for TicketStatus {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.as_str())
        }
    }

    impl FromStr for TicketStatus {
        type Err = &'static str;

        fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
            match s.trim().trim_matches('"').to_ascii_lowercase().as_str() {
                "open" => Ok(Self::Open),
                "in_progress" => Ok(Self::InProgress),
                "resolved" => Ok(Self::Resolved),
                "closed" => Ok(Self::Closed),
                _ => Err("status must be open|in_progress|resolved|closed"),
            }
        }
    }

    #[derive(Debug, Clone)]
    pub struct Ticket {
        pub id: i64,
        pub title: String,
        pub description: String,
        pub status: TicketStatus,
        pub priority: i64,
        pub parent_id: Option<i64>,
        pub requirement_id: Option<i64>,
        pub mode: Option<String>,
        pub resolution: Option<String>,
        pub created_at: String,
        pub updated_at: String,
        pub path: PathBuf,
    }

    impl Ticket {
        pub fn display_path(&self, workspace: &Path) -> String {
            relative_path(workspace, &self.path)
        }
    }

    fn now_secs() -> String {
        crate::storage::util::unix_secs().to_string()
    }

    fn title_for(raw: &crate::storage::util::MarkdownDocument, fallback: &str) -> String {
        nonempty_meta(&raw.meta, "title")
            .or_else(|| raw.heading.clone())
            .unwrap_or_else(|| {
                raw.path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(fallback)
                    .to_string()
            })
    }

    fn render(t: &Ticket) -> String {
        let mut s = String::new();
        s.push_str("---\n");
        let _ = writeln!(s, "id: {}", t.id);
        let _ = writeln!(s, "status: {}", t.status);
        let _ = writeln!(s, "priority: {}", t.priority);
        if let Some(p) = t.parent_id {
            let _ = writeln!(s, "parent: {p}");
        }
        if let Some(r) = t.requirement_id {
            let _ = writeln!(s, "requirement: {r}");
        }
        if let Some(m) = &t.mode {
            let _ = writeln!(s, "mode: {m}");
        }
        if let Some(r) = &t.resolution {
            let _ = writeln!(s, "resolution: {r}");
        }
        let _ = writeln!(s, "created: {}", t.created_at);
        let _ = writeln!(s, "updated: {}", t.updated_at);
        s.push_str("---\n\n");
        let _ = writeln!(s, "# {}", t.title);
        if !t.description.trim().is_empty() {
            s.push('\n');
            s.push_str(t.description.trim());
            s.push('\n');
        }
        s
    }

    fn write_at(path: &Path, t: &Ticket) -> Result<()> {
        write_markdown(path, &render(t))
    }

    pub fn path_for(cfg: &Config, workspace: &Path, t: &Ticket) -> PathBuf {
        cfg.tickets_path(workspace)
            .join(format!("{}-{}.md", t.id, slugify(&t.title, "ticket")))
    }

    pub fn load_all(cfg: &Config, workspace: &Path) -> Result<Vec<Ticket>> {
        let mut raws = load_markdown_dir(&cfg.tickets_path(workspace))?;
        let assigned = assign_missing_ids(&mut raws);

        let mut out = Vec::new();
        for (raw, assigned_id) in raws.into_iter().zip(assigned) {
            let id = meta_i64(&raw.meta, "id").expect("assign_missing_ids must populate ids");
            let title = title_for(&raw, "ticket");
            let status = nonempty_meta(&raw.meta, "status")
                .and_then(|v| v.parse().ok())
                .unwrap_or_default();
            let priority = meta_i64(&raw.meta, "priority").unwrap_or(2).clamp(1, 3);
            let parent_id = meta_i64(&raw.meta, "parent");
            let requirement_id = meta_i64(&raw.meta, "requirement");
            let mode = nonempty_meta(&raw.meta, "mode");
            let resolution = nonempty_meta(&raw.meta, "resolution");
            let created_at = nonempty_meta(&raw.meta, "created").unwrap_or_else(now_secs);
            let updated_at =
                nonempty_meta(&raw.meta, "updated").unwrap_or_else(|| created_at.clone());

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
                path: raw.path.clone(),
            };
            if assigned_id {
                write_at(&raw.path, &ticket)?;
            }
            out.push(ticket);
        }

        out.sort_by_key(|t| (t.priority, t.id));
        Ok(out)
    }

    pub fn load_by_id(cfg: &Config, workspace: &Path, id: i64) -> Result<Option<Ticket>> {
        Ok(load_all(cfg, workspace)?.into_iter().find(|t| t.id == id))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create(
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
        let id = load_all(cfg, workspace)?
            .iter()
            .map(|ticket| ticket.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let mut ticket = Ticket {
            id,
            title: title.to_string(),
            description: description.to_string(),
            status: TicketStatus::Open,
            priority: priority.clamp(1, 3),
            parent_id,
            requirement_id,
            mode: (!mode.trim().is_empty()).then(|| mode.to_string()),
            resolution: None,
            created_at: now.clone(),
            updated_at: now,
            path: PathBuf::new(),
        };
        let path = path_for(cfg, workspace, &ticket);
        ticket.path = path.clone();
        write_at(&path, &ticket)?;
        Ok(ticket)
    }

    pub fn update(
        cfg: &Config,
        workspace: &Path,
        id: i64,
        edit: &TicketEdit,
        status: Option<TicketStatus>,
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
        edit.parent_id.apply_to(&mut ticket.parent_id);
        edit.requirement_id.apply_to(&mut ticket.requirement_id);
        if let Some(status) = status {
            ticket.status = status;
        }
        if let Some(r) = resolution {
            ticket.resolution = Some(r.to_string());
        }
        ticket.updated_at = now_secs();

        let old_path = ticket.path.clone();
        let new_path = path_for(cfg, workspace, &ticket);
        ticket.path = new_path.clone();
        replace_backing_file(&old_path, &new_path, &render(&ticket))?;
        Ok(Some(ticket))
    }

    pub fn sync(cfg: &Config, workspace: &Path) -> Result<usize> {
        Ok(load_all(cfg, workspace)?.len())
    }
}
pub mod prompts {
    use anyhow::Result;

    use crate::storage::db::Db;
    use crate::storage::modes::Mode;

    pub fn seed_prompts(db: &Db) -> Result<()> {
        for mode in Mode::all()
            .into_iter()
            .filter(|mode| mode.allows_extended())
        {
            if db.prompt_active(mode.as_str())?.is_none() {
                db.prompt_add_version(mode.as_str(), "", "default", "initial seed")?;
            }
        }
        Ok(())
    }

    pub fn load_extended(db: &Db, mode: Mode) -> Result<String> {
        if !mode.allows_extended() {
            return Ok(String::new());
        }
        Ok(db
            .prompt_active(mode.as_str())?
            .map(|p| p.content)
            .unwrap_or_default())
    }
}
