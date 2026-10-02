pub mod context {
    use anyhow::Result;
    use serde_json::{Value, json};

    use crate::llm::{self, ChatMessage, LlmClient};
    use crate::tools::ToolSpec;

    /// Approximate token cost of the tool definitions sent with every request.
    pub fn estimate_tools(tools: &[Value]) -> i64 {
        let chars: usize = tools.iter().map(|t| t.to_string().chars().count()).sum();
        ((chars / 4) + 4) as i64
    }

    /// Estimated token cost of each part of the prompt.
    #[derive(Debug, Clone, Copy, Default)]
    pub struct ContextInfo {
        pub system_prompt_tokens: i64,
        pub system_tools_tokens: i64,
        pub turn_messages_tokens: i64,
        pub total_tokens: i64,
        pub context_window: i64,
    }

    impl ContextInfo {
        /// Measure a context. Read-only: it borrows the existing strings and does
        /// not allocate or clone them.
        pub fn compute(messages: &[ChatMessage], tools: &[Value], context_window: i64) -> Self {
            let system_prompt_tokens = messages.first().map(|m| m.est_tokens()).unwrap_or(0);
            let turn_messages_tokens = messages.iter().skip(1).map(|m| m.est_tokens()).sum();
            let system_tools_tokens = estimate_tools(tools);
            Self {
                system_prompt_tokens,
                system_tools_tokens,
                turn_messages_tokens,
                total_tokens: system_prompt_tokens + system_tools_tokens + turn_messages_tokens,
                context_window,
            }
        }

        /// Rebuild from a JSON form (see [`ContextInfo::to_json`]).
        pub fn from_json(v: &Value) -> Self {
            let get = |k: &str| v.get(k).and_then(|x| x.as_i64()).unwrap_or(0);
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

        /// Human-readable size block shared by `/context` and `genji inspect`.
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

    /// Record of one compaction, returned to the agent so it can persist and emit
    /// the outcome (the composer only owns the in-memory conversation).
    #[derive(Debug, Clone)]
    pub struct Compaction {
        pub removed: i64,
        pub before: i64,
        pub after: i64,
        pub summary: String,
        pub prompt_tokens: i64,
        pub completion_tokens: i64,
    }

    /// Owns the model-facing context and the operations that shape it.
    pub struct ContextComposer {
        messages: Vec<ChatMessage>,
        tools: Vec<ToolSpec>,
        context_window: i64,
        last_prompt_tokens: i64,
    }

    impl ContextComposer {
        /// Start a conversation with a system prompt and the mode's tools.
        pub fn new(system: String, tools: Vec<ToolSpec>, context_window: i64) -> Self {
            Self {
                messages: vec![ChatMessage::system(system)],
                tools,
                context_window,
                last_prompt_tokens: 0,
            }
        }

        pub fn messages(&self) -> &[ChatMessage] {
            &self.messages
        }

        /// The mode's tool definitions, ready to send to the provider.
        pub fn tools_json(&self) -> Vec<Value> {
            self.tools.iter().map(|t| t.to_json()).collect()
        }

        pub fn set_last_prompt_tokens(&mut self, tokens: i64) {
            self.last_prompt_tokens = tokens;
        }

        /// Append a message to the conversation.
        pub fn push(&mut self, msg: ChatMessage) {
            self.messages.push(msg);
        }

        /// Append a tool result, pairing it with the call being answered.
        pub fn push_tool_result(&mut self, tool_call_id: &str, content: impl Into<String>) {
            self.messages
                .push(ChatMessage::tool_result(tool_call_id, content));
        }

        /// Replace the system prompt in place, preserving the turns.
        pub fn set_system(&mut self, system: String) {
            if let Some(first) = self.messages.first_mut() {
                first.role = "system".into();
                first.content = system;
            } else {
                self.messages.push(ChatMessage::system(system));
            }
        }

        /// Switch the mode-dependent context: its tools, system prompt, and window.
        pub fn switch_mode(&mut self, tools: Vec<ToolSpec>, system: String, context_window: i64) {
            self.tools = tools;
            self.context_window = context_window;
            self.set_system(system);
        }

        /// Measure the current context. Called only when someone asks (via
        /// `/context` or `genji inspect`); it reads the existing strings and
        /// allocates nothing beyond the tool JSON it must serialize.
        pub fn stats(&self) -> ContextInfo {
            ContextInfo::compute(&self.messages, &self.tools_json(), self.context_window)
        }

        pub fn snapshot(&self) -> Value {
            json!({
                "context_window": self.context_window,
                "last_prompt_tokens": self.last_prompt_tokens,
                "messages": self.messages.iter().map(|m| m.to_json()).collect::<Vec<_>>(),
                "tools": self.tools_json(),
            })
        }

        /// Compact when the estimated prompt size reaches `threshold_fraction` of
        /// the context window. Returns the compaction outcome when one happened.
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

        /// Summarize the middle of the conversation and replace it with a single
        /// summary message, keeping the system prompt and the most recent turns.
        /// Tool-call/result pairs are never split.
        pub fn compact(&mut self, keep: usize, llm: &LlmClient) -> Result<Option<Compaction>> {
            let keep = keep.max(2);
            if self.messages.len() <= keep + 2 {
                return Ok(None);
            }
            let mut split = self.messages.len() - keep;
            // Never start the kept window with an orphaned tool result.
            while split < self.messages.len() && self.messages[split].role == "tool" {
                split += 1;
            }
            if split <= 1 {
                return Ok(None);
            }
            let before = llm::estimate_messages(&self.messages);
            let middle: Vec<ChatMessage> = self.messages[1..split].to_vec();
            let rendered = llm::truncate(render_messages(&middle), 120_000);
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
            let removed = (split - 1) as i64;
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
                    .map(|c| format!("{}({})", c.name, llm::truncate(c.arguments.clone(), 200)))
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
        use crate::llm::ChatMessage;
        use crate::tools::ToolSpec;
        use serde_json::json;

        fn tool(name: &'static str) -> ToolSpec {
            ToolSpec {
                name,
                description: "d",
                parameters: json!({"type": "object"}),
            }
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
            ctx.push_tool_result("call_1", "result");
            assert_eq!(ctx.messages().len(), 3);
            assert_eq!(ctx.messages()[2].role, "tool");

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
            // No token math is part of the snapshot.
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
            assert_eq!(ctx.tools_json()[0]["function"]["name"], "bash");
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
}
pub mod events {
    //! Machine-readable event stream.
    //!
    //! A machine frontend can consume genji's run as newline-delimited JSON
    //! (JSONL) from **stdout** or the append-only per-instance event file.
    //! Everything a human reads (progress narration,
    //! warnings, retries, budget notices, …) goes to **stderr** and is cosmetic.
    //!
    //! The event file is the durable complete output, including for subagent runs
    //! whose events are not relayed into the parent's stream. See `genji inspect`.

    use serde_json::{Value, json};
    use std::io::{self, Write};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Writes a newline-delimited JSON event stream (one object per line, flushed
    /// after each event) to stdout and the event file.
    pub struct EventEmitter {
        instance: String,
        seq: AtomicU64,
        out: Mutex<Box<dyn Write + Send>>,
        trace: Mutex<Option<std::fs::File>>,
    }

    impl EventEmitter {
        /// Emit to stdout, tagged with `instance`, and append to `trace_path`.
        /// Event-file creation is fail-fast: a run must not start without its
        /// durable output.
        pub fn new(instance: impl Into<String>, trace_path: Option<PathBuf>) -> io::Result<Self> {
            Self::with_writer_and_trace(instance, Box::new(io::stdout()), trace_path)
        }

        /// Construct an emitter writing only to a custom sink (used by tests).
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
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0)
        }

        /// Emit one event. Adds `seq`/`ts` (and `instance`, when non-empty and not
        /// already present) so consumers can order and group events.
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
            // Persist first: stdout and socket delivery are live conveniences, but
            // the append-only event file is the durable output for the run.
            if let Ok(mut trace) = self.trace.lock()
                && let Some(f) = trace.as_mut()
                && writeln!(f, "{line}").and_then(|_| f.flush()).is_err()
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
            // Tool arguments arrive as a JSON-encoded string. Emit them parsed in
            // the common case and fall back to the raw string only when they are
            // not valid JSON, so the event stays terse.
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

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Mode {
        Plan,
        Build,
        Explore,
        Retro,
    }

    impl Mode {
        pub fn as_str(&self) -> &'static str {
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

        /// Minimal, non-editable core system prompt for the mode.
        pub fn core_prompt(&self) -> &'static str {
            match self {
                Mode::Plan => include_str!("prompts/plan.md"),
                Mode::Build => include_str!("prompts/build.md"),
                Mode::Explore => include_str!("prompts/explore.md"),
                Mode::Retro => include_str!("prompts/retro.md"),
            }
        }

        /// Whether this mode has a user-editable extended prompt. RETRO is
        /// intentionally fixed: it must not be able to extend or rewrite its own
        /// instructions, directly or via a spawned agent.
        pub fn allows_extended(&self) -> bool {
            !matches!(self, Mode::Retro)
        }

        /// Extra system-prompt guidance appended when the Formal flag is on. Empty for
        /// modes that never touch the requirements/tickets system.
        pub fn formal_guidance(&self) -> &'static str {
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

    /// Run a program, capturing stdout/stderr to temp files and enforcing a
    /// timeout. This avoids pipe-buffer deadlocks and bounds memory.
    pub fn run_capture(
        program: &str,
        args: &[String],
        cwd: &Path,
        timeout: Duration,
        max_read_bytes: usize,
    ) -> Result<ProcResult> {
        let tmpdir = cwd.join(".genji").join("tmp");
        std::fs::create_dir_all(&tmpdir).ok();
        let out_path = tmp_path(&tmpdir, "out");
        let err_path = tmp_path(&tmpdir, "err");
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

    /// Convenience for running a shell command string via `bash -c`.
    pub fn run_bash(
        command: &str,
        cwd: &Path,
        timeout: Duration,
        max_read_bytes: usize,
    ) -> Result<ProcResult> {
        run_capture(
            "bash",
            &["-c".to_string(), command.to_string()],
            cwd,
            timeout,
            max_read_bytes,
        )
    }
}
pub mod registry {
    //! A lightweight registry of running genji instances.
    //!
    //! Every top-level run that opens a control socket writes a small JSON record
    //! to a per-user directory. `genji list` / `stop` / `instruct` / `inspect` read
    //! those records to find and steer instances, even from another workspace.
    //!
    //! Records are best-effort: a crashed process may leave a stale file behind,
    //! which is detected by probing the control socket and cleaned up on the next
    //! listing.

    use anyhow::{Context, Result, bail};
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Instance {
        /// The single id for this run. It names the registry record, the event trace
        /// in [`events_dir`], the DB record and every event's `instance` field.
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

        /// True while the instance's control socket still accepts connections.
        pub fn is_live(&self) -> bool {
            crate::socket::send(Path::new(&self.control_socket), "/ping")
                .map(|r| !r.trim().is_empty())
                .unwrap_or(false)
        }
    }

    /// Directory holding instance records. `GENJI_REGISTRY_DIR` overrides it
    /// (handy for tests); otherwise it lives under the user's home directory.
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

    /// Directory holding per-instance event traces (`<instance>.jsonl`). These live
    /// alongside the instance records so any run — including a finished subagent
    /// whose events were never relayed — can be inspected from anywhere.
    pub fn events_dir() -> PathBuf {
        dir().join("events")
    }

    /// Trace file for one instance. The id → path mapping lives here so every
    /// caller (run setup, `inspect`, subagent relay) agrees on it.
    pub fn events_path(id: &str) -> PathBuf {
        events_dir().join(format!("{id}.jsonl"))
    }

    pub fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
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
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let mut x = candidate(nanos ^ ((std::process::id() as u64) << 21));
        let d = dir();
        for _ in 0..64 {
            let id = format!("{:06x}", x & 0x00ff_ffff);
            if !d.join(format!("{id}.json")).exists() {
                return id;
            }
            x = candidate(x);
        }
        format!("{:08x}", (nanos as u32) ^ std::process::id())
    }

    /// Parse every valid record on disk (invalid files are dropped). Does not probe
    /// liveness.
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

    /// Live instances, pruning stale records as a side effect.
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

    /// Resolve an instance id against the live pool, leniently.
    ///
    /// An exact id always wins. Otherwise a unique id prefix is accepted, so
    /// `genji instruct abc` resolves `abc123` when it is the only live instance
    /// starting with `abc`. An ambiguous prefix reports the candidates instead of
    /// guessing, and a prefix that matches nothing is treated as unknown.
    pub fn find(id: &str) -> Result<Instance> {
        let id = id.trim();

        // `list_live` prunes stale records as a side effect; it is the single
        // source of liveness, so reuse it rather than probing again here.
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
    //! Small shared helpers.

    /// Lowercase ASCII slug: runs of non-alphanumerics collapse to a single `-`,
    /// trimmed at both ends and capped at 60 chars. `fallback` is used when the
    /// result would otherwise be empty (for example a title of `"!!!"`).
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
        use super::slugify;

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
    //!
    //! Requirements are markdown files directly under `.genji/requirements/`:
    //!
    //! ```text
    //! .genji/requirements/
    //!   1-cat-image-classifier.md
    //!   2-accept-image-files-and-urls.md
    //! ```
    //!
    //! Metadata lives in simple `key: value` frontmatter (`id`, `level`, `status`,
    //! `parent`, `source`, `created`, `updated`); the text after the first
    //! `# Heading` is the requirement body. The level (`stakeholder` or `system`)
    //! is read from the frontmatter and defaults to `stakeholder` when absent.

    use anyhow::{Context, Result, bail};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::Config;
    use crate::storage::util::slugify;

    #[derive(Debug, Clone)]
    pub struct Requirement {
        pub id: i64,
        pub level: String,
        pub title: String,
        pub body: String,
        pub status: String,
        pub parent_id: Option<i64>,
        pub source: String,
        pub path: PathBuf,
        pub created_at: String,
        pub updated_at: String,
    }

    impl Requirement {
        pub fn display_path(&self, workspace: &Path) -> String {
            rel_to(workspace, &self.path)
        }
    }

    fn now_secs() -> String {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
            .to_string()
    }

    fn valid_level(v: &str) -> Option<String> {
        let v = v.trim().trim_matches('"').to_ascii_lowercase();
        if v == "stakeholder" || v == "system" {
            Some(v)
        } else {
            None
        }
    }

    /// Split leading `---` frontmatter from the markdown body.
    fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
        let mut meta = BTreeMap::new();
        if let Some(rest) = text.strip_prefix("---\n")
            && let Some(idx) = rest.find("\n---")
        {
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
        (meta, text.to_string())
    }

    /// Remove the first level-1 heading and return it separately.
    fn split_heading(text: &str) -> (Option<String>, String) {
        let mut heading = None;
        let mut lines: Vec<&str> = Vec::new();
        for line in text.lines() {
            if heading.is_none()
                && let Some(h) = line.strip_prefix("# ")
            {
                heading = Some(h.trim().to_string());
                continue;
            }
            lines.push(line);
        }
        (heading, lines.join("\n").trim().to_string())
    }

    fn render(r: &Requirement) -> String {
        let mut s = String::new();
        s.push_str("---\n");
        s.push_str(&format!("id: {}\n", r.id));
        s.push_str(&format!("level: {}\n", r.level));
        s.push_str(&format!("status: {}\n", r.status));
        if let Some(p) = r.parent_id {
            s.push_str(&format!("parent: {p}\n"));
        }
        s.push_str(&format!("source: {}\n", r.source));
        s.push_str(&format!("created: {}\n", r.created_at));
        s.push_str(&format!("updated: {}\n", r.updated_at));
        s.push_str("---\n\n");
        s.push_str(&format!("# {}\n", r.title));
        if !r.body.trim().is_empty() {
            s.push('\n');
            s.push_str(r.body.trim());
            s.push('\n');
        }
        s
    }

    fn write_at(path: &Path, r: &Requirement) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, render(r)).with_context(|| format!("writing {}", path.display()))?;
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

    /// Where a requirement's file lives: `<root>/<id>-<slug>.md`.
    fn path_for(cfg: &Config, workspace: &Path, r: &Requirement) -> PathBuf {
        cfg.requirements_path(workspace).join(format!(
            "{}-{}.md",
            r.id,
            slugify(&r.title, "requirement")
        ))
    }

    /// Load every requirement file. Files missing an `id` are assigned one and
    /// rewritten so identity is stable across runs.
    pub fn load_all(cfg: &Config, workspace: &Path) -> Result<Vec<Requirement>> {
        let root = cfg.requirements_path(workspace);
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
            let level = raw
                .meta
                .get("level")
                .and_then(|v| valid_level(v))
                .unwrap_or_else(|| "stakeholder".into());
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
                        .unwrap_or("requirement")
                        .to_string()
                });
            let status = raw
                .meta
                .get("status")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| "active".into());
            let parent_id = raw.meta.get("parent").and_then(|v| v.parse::<i64>().ok());
            let source = raw
                .meta
                .get("source")
                .filter(|s| !s.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| "user_md".into());
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
            // Persist a newly assigned id (or fill in missing metadata) once.
            if !had_id {
                write_at(&raw.path, &req)?;
            }
            out.push(req);
        }

        out.sort_by(|a, b| {
            let rank = |r: &Requirement| if r.level == "stakeholder" { 0 } else { 1 };
            rank(a).cmp(&rank(b)).then(a.id.cmp(&b.id))
        });
        Ok(out)
    }

    pub fn load_by_id(cfg: &Config, workspace: &Path, id: i64) -> Result<Option<Requirement>> {
        Ok(load_all(cfg, workspace)?.into_iter().find(|r| r.id == id))
    }

    pub fn active_count(cfg: &Config, workspace: &Path) -> Result<i64> {
        Ok(load_all(cfg, workspace)?
            .iter()
            .filter(|r| r.status == "active")
            .count() as i64)
    }

    pub fn create(
        cfg: &Config,
        workspace: &Path,
        level: &str,
        title: &str,
        body: &str,
        parent_id: Option<i64>,
        source: &str,
    ) -> Result<Requirement> {
        let Some(level) = valid_level(level) else {
            bail!("level must be `stakeholder` or `system`");
        };
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
            status: "active".into(),
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
        status: Option<&str>,
        level: Option<&str>,
        parent_id: Option<Option<i64>>,
    ) -> Result<bool> {
        let mut req = match load_by_id(cfg, workspace, id)? {
            Some(r) => r,
            None => return Ok(false),
        };
        if let Some(l) = level {
            let Some(l) = valid_level(l) else {
                bail!("level must be stakeholder|system");
            };
            req.level = l;
        }
        if let Some(t) = title {
            req.title = t.to_string();
        }
        if let Some(b) = body {
            req.body = b.to_string();
        }
        if let Some(s) = status {
            req.status = s.to_string();
        }
        if let Some(p) = parent_id {
            req.parent_id = p;
        }
        req.updated_at = now_secs();

        let old_path = req.path.clone();
        let new_path = path_for(cfg, workspace, &req);
        req.path = new_path.clone();
        write_at(&new_path, &req)?;
        if new_path != old_path && old_path.exists() {
            let _ = std::fs::remove_file(&old_path);
        }
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
        update(cfg, workspace, id, None, None, Some("removed"), None, None)
    }

    /// Load and normalize Markdown requirements. Markdown is the only source of
    /// truth; there is intentionally no database migration path.
    pub fn sync(cfg: &Config, workspace: &Path) -> Result<usize> {
        Ok(load_all(cfg, workspace)?.len())
    }

    #[cfg(test)]
    mod tests {
        use super::{
            active_count, create, load_all, load_by_id, remove, split_frontmatter, split_heading,
            update,
        };
        use crate::config::Config;
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
            assert_eq!(a.level, "system");
            assert_eq!(b.level, "stakeholder");

            let _ = std::fs::remove_dir_all(&ws);
        }

        #[test]
        fn heading_extraction() {
            assert_eq!(
                split_heading("intro\n# Real Title\nbody"),
                (Some("Real Title".into()), "intro\nbody".into())
            );
            assert_eq!(split_heading("no heading"), (None, "no heading".into()));
        }

        #[test]
        fn frontmatter_parsing() {
            let (meta, body) = split_frontmatter("---\nid: 4\nlevel: system\n---\n# T\nbody\n");
            assert_eq!(meta.get("id").map(String::as_str), Some("4"));
            assert_eq!(meta.get("level").map(String::as_str), Some("system"));
            assert_eq!(body, "# T\nbody\n");
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
                .map(|d| d.as_nanos())
                .unwrap_or(0);
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
                "stakeholder",
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
                "system",
                "Accept URLs",
                "Accept image URLs.",
                Some(1),
                "agent",
            )
            .unwrap();
            assert_eq!(r2.id, 2);
            assert_eq!(active_count(&cfg, &ws).unwrap(), 2);

            // Editing the title renames the backing file.
            assert!(
                update(
                    &cfg,
                    &ws,
                    2,
                    Some("Accept Files and URLs"),
                    None,
                    Some("met"),
                    None,
                    None
                )
                .unwrap()
            );
            let r2b = load_by_id(&cfg, &ws, 2).unwrap().unwrap();
            assert_eq!(r2b.title, "Accept Files and URLs");
            assert_eq!(r2b.status, "met");
            assert_eq!(r2b.parent_id, Some(1));
            assert!(
                r2b.display_path(&ws)
                    .ends_with("2-accept-files-and-urls.md")
            );
            assert_eq!(active_count(&cfg, &ws).unwrap(), 1);

            update(&cfg, &ws, 2, None, None, None, Some("stakeholder"), None).unwrap();
            let r2c = load_by_id(&cfg, &ws, 2).unwrap().unwrap();
            assert_eq!(r2c.level, "stakeholder");
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
            assert_eq!(all[0].level, "stakeholder");
            // The id is persisted back into the file.
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
    //!
    //! All tickets live as Markdown files directly under `.genji/tickets/`:
    //!
    //! ```text
    //! .genji/tickets/
    //!   7-add-rate-limit.md
    //!   8-write-tests.md
    //! ```
    //!
    //! Metadata lives in simple `key: value` frontmatter (`id`, `status`,
    //! `priority`, `parent`, `requirement`, `mode`, `created`, `updated`); the text
    //! after the first `# Heading` is the ticket description. Status and
    //! resolution remain in the Markdown frontmatter; there is no database archive
    //! or migration path.

    use anyhow::{Context, Result};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::config::Config;
    use crate::storage::db::TicketEdit;
    use crate::storage::util::slugify;

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
            self.path.as_ref().map(|p| rel_to(workspace, p))
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
        if let Some(rest) = text.strip_prefix("---\n")
            && let Some(idx) = rest.find("\n---")
        {
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
        (meta, text.to_string())
    }

    /// Remove the first level-1 heading and return it separately.
    fn split_heading(text: &str) -> (Option<String>, String) {
        let mut heading = None;
        let mut lines: Vec<&str> = Vec::new();
        for line in text.lines() {
            if heading.is_none()
                && let Some(h) = line.strip_prefix("# ")
            {
                heading = Some(h.trim().to_string());
                continue;
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
        if let Some(m) = &t.mode
            && !m.trim().is_empty()
        {
            s.push_str(&format!("mode: {m}\n"));
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
        Ok(load_all(cfg, workspace)?.into_iter().find(|t| t.id == id))
    }

    /// Next ticket id from the Markdown store.
    pub fn next_id(cfg: &Config, workspace: &Path) -> Result<i64> {
        Ok(load_all(cfg, workspace)?
            .iter()
            .map(|t| t.id)
            .max()
            .unwrap_or(0)
            + 1)
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
        let mut ticket = Ticket {
            id: next_id(cfg, workspace)?,
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
        if let Some(old) = old_path
            && old != new_path
            && old.exists()
        {
            let _ = std::fs::remove_file(&old);
        }
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
        for mode in Mode::all().into_iter().filter(Mode::allows_extended) {
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
