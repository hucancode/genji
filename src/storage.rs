/// A string-backed enum with `as_str`, `Display`, lenient `FromStr`, and serde
/// (de)serialization through the same string forms.
macro_rules! string_enum {
    ($(#[$m:meta])* pub enum $name:ident { $($(#[$vm:meta])* $variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$m])*
        pub enum $name { $($(#[$vm])* $variant),+ }

        impl $name {
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $s),+ }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = String;

            fn from_str(s: &str) -> std::result::Result<Self, String> {
                match s.trim().trim_matches('"').to_ascii_lowercase().as_str() {
                    $($s => Ok(Self::$variant),)+
                    other => Err(format!(
                        "unknown {} `{other}` (expected {})",
                        stringify!($name),
                        [$($s),+].join("|")
                    )),
                }
            }
        }

        impl serde::Serialize for $name {
            fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
                String::deserialize(d)?.parse().map_err(serde::de::Error::custom)
            }
        }
    };
}
pub(crate) use string_enum;

pub mod context {
    use anyhow::Result;
    use serde_json::{Value, json};
    use std::fmt::Write as _;

    use crate::llm::{self, ChatMessage, LlmClient, Role};

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
        /// Message count when `last_prompt_tokens` was measured.
        prompt_len: usize,
    }

    impl ContextComposer {
        pub fn new(system: String, tools: Vec<Value>, context_window: i64) -> Self {
            Self {
                messages: vec![ChatMessage::system(system)],
                tools,
                context_window,
                last_prompt_tokens: 0,
                prompt_len: 0,
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
            self.prompt_len = self.messages.len();
        }

        /// The last measured prompt size plus an estimate for messages added since.
        fn est_tokens(&self) -> i64 {
            if self.last_prompt_tokens > 0 {
                self.last_prompt_tokens + llm::estimate_messages(&self.messages[self.prompt_len..])
            } else {
                llm::estimate_messages(&self.messages)
            }
        }

        pub fn push(&mut self, msg: ChatMessage) {
            self.messages.push(msg);
        }

        pub fn set_system(&mut self, system: String) {
            self.messages[0].content = system;
        }

        #[cfg(any(feature = "formal", test))]
        pub fn switch_mode(&mut self, tools: Vec<Value>, system: String, context_window: i64) {
            self.tools = tools;
            self.context_window = context_window;
            self.set_system(system);
        }

        pub fn snapshot(&self) -> Value {
            json!({
                "context_window": self.context_window,
                "last_prompt_tokens": self.last_prompt_tokens,
                "messages": self.messages,
                "tools": self.tools,
            })
        }

        pub fn maybe_compact(
            &mut self,
            threshold_fraction: f64,
            keep: usize,
            llm: &LlmClient,
        ) -> Result<Option<Compaction>> {
            let threshold = (self.context_window as f64 * threshold_fraction) as i64;
            if self.est_tokens() >= threshold {
                self.compact(keep, llm)
            } else {
                Ok(None)
            }
        }

        pub fn compact(&mut self, keep: usize, llm: &LlmClient) -> Result<Option<Compaction>> {
            let keep = keep.max(2);
            let len = self.messages.len();
            if len <= keep + 2 {
                return Ok(None);
            }
            let mut split = len - keep;
            while split < len && self.messages[split].role == Role::Tool {
                split += 1;
            }
            let before = llm::estimate_messages(&self.messages);
            let rendered_source = render_messages(&self.messages[1..split]);
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

            let recent = self.messages.split_off(split);
            self.messages.truncate(1);
            self.messages.push(ChatMessage::user(format!(
                "[compacted summary of earlier conversation]\n{summary}"
            )));
            self.messages.extend(recent);
            self.last_prompt_tokens = 0;
            self.prompt_len = 0;
            Ok(Some(Compaction {
                removed: i64::try_from(split - 1).unwrap_or(i64::MAX),
                before,
                after: llm::estimate_messages(&self.messages),
                summary,
                prompt_tokens: resp.prompt_tokens,
                completion_tokens: resp.completion_tokens,
            }))
        }
    }

    fn render_messages(msgs: &[ChatMessage]) -> String {
        let mut out = String::new();
        for m in msgs {
            let _ = write!(out, "[{}] {}", m.role, m.content);
            if !m.tool_calls.is_empty() {
                let calls: Vec<String> = m
                    .tool_calls
                    .iter()
                    .map(|c| format!("{}({})", c.name, llm::truncate(&c.arguments, 200)))
                    .collect();
                let _ = write!(out, "\n  calls: {}", calls.join(", "));
            }
            if let Some(id) = &m.tool_call_id {
                let _ = write!(out, " (tool_call_id={id})");
            }
            out.push_str("\n\n");
        }
        out
    }

    #[cfg(test)]
    mod tests {
        use super::ContextComposer;
        use crate::llm::ChatMessage;
        use serde_json::json;

        fn tool(name: &str) -> serde_json::Value {
            json!({"type": "function", "function": {"name": name}})
        }

        #[test]
        fn snapshot_returns_live_prompt() {
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
        }

        #[test]
        fn estimate_counts_messages_after_last_prompt() {
            let mut ctx = ContextComposer::new("sys".to_string(), Vec::new(), 1000);
            ctx.set_last_prompt_tokens(100);
            assert_eq!(ctx.est_tokens(), 108);
            ctx.push(ChatMessage::tool_result("c1", "x".repeat(4000)));
            assert_eq!(ctx.est_tokens(), 100 + 1004 + 8);
        }

        #[test]
        fn switch_mode_replaces_system_and_tools() {
            let mut ctx = ContextComposer::new("old".to_string(), vec![tool("read")], 1000);
            ctx.push(ChatMessage::user("keep me"));
            ctx.switch_mode(vec![tool("bash")], "new".to_string(), 2000);
            assert_eq!(ctx.messages()[0].content, "new");
            assert_eq!(ctx.messages()[1].content, "keep me");
            assert_eq!(ctx.tools()[0]["function"]["name"], "bash");
            assert_eq!(ctx.snapshot()["context_window"].as_i64(), Some(2000));
        }
    }
}
pub mod db {
    use anyhow::{Context, Result};
    use rusqlite::{Connection, params};
    use serde_json::{Value, json};
    use std::path::Path;

    const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

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
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;",
            )?;
            conn.execute_batch(include_str!("sql/schema.sql"))?;
            Ok(Db { conn })
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

        #[cfg(feature = "formal")]
        pub fn instance_set_mode(&self, id: &str, mode: &str) -> Result<()> {
            self.conn
                .execute("UPDATE instances SET mode=? WHERE id=?", params![mode, id])?;
            Ok(())
        }

        pub fn instance_set_tokens(&self, id: &str, tokens: i64) -> Result<()> {
            self.conn.execute(
                "UPDATE instances SET tokens_used=? WHERE id=?",
                params![tokens, id],
            )?;
            Ok(())
        }

        /// Summary of the single instance whose id starts with `prefix`.
        pub fn instance_summary(&self, prefix: &str) -> Result<Value> {
            let mut stmt = self.conn.prepare(
                "SELECT i.id,i.mode,i.model,i.parent_instance,i.depth,i.task,i.status,i.tokens_used,
                        i.started_at,i.ended_at,i.report,
                        (SELECT COUNT(*) FROM messages m WHERE m.instance_id=i.id)
                 FROM instances i WHERE i.id LIKE ?1 || '%' ORDER BY i.id LIMIT 2",
            )?;
            let mut rows = stmt
                .query_map(params![prefix], |r| {
                    Ok(json!({
                        "id": r.get::<_, String>(0)?,
                        "mode": r.get::<_, String>(1)?,
                        "model": r.get::<_, Option<String>>(2)?,
                        "parent": r.get::<_, Option<String>>(3)?,
                        "depth": r.get::<_, i64>(4)?,
                        "task": r.get::<_, Option<String>>(5)?,
                        "status": r.get::<_, String>(6)?,
                        "tokens_used": r.get::<_, i64>(7)?,
                        "started_at": r.get::<_, String>(8)?,
                        "ended_at": r.get::<_, Option<String>>(9)?,
                        "report": r.get::<_, Option<String>>(10)?,
                        "messages": r.get::<_, i64>(11)?,
                    }))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            match rows.len() {
                1 => Ok(rows.remove(0)),
                0 => anyhow::bail!("no recorded instance with id `{prefix}`"),
                _ => anyhow::bail!("instance id `{prefix}` is ambiguous (use a longer prefix)"),
            }
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
        ) -> Result<()> {
            self.conn
                .prepare_cached(
                    "INSERT INTO messages(instance_id,seq,role,content,tool_calls,tool_call_id,reasoning) VALUES(?,?,?,?,?,?,?)",
                )?
                .execute(params![instance_id, seq, role, content, tool_calls, tool_call_id, reasoning])?;
            Ok(())
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
            self.conn
                .prepare_cached(
                    "INSERT INTO tool_calls(instance_id,message_seq,name,args,result,is_error,duration_ms) VALUES(?,?,?,?,?,?,?)",
                )?
                .execute(params![instance_id, message_seq, name, args, result, i64::from(is_error), duration_ms])?;
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
    //! Machine-readable event stream (JSON lines on stdout).

    use serde_json::{Value, json};
    use std::io::{self, Write};
    use std::sync::Mutex;

    pub struct EventEmitter {
        instance: String,
        /// Sequence counter and sink, locked together so `seq` matches line order.
        out: Mutex<(u64, Box<dyn Write + Send>)>,
    }

    impl EventEmitter {
        pub fn new(instance: impl Into<String>) -> Self {
            Self::with_writer(instance, Box::new(io::stdout()))
        }

        pub fn with_writer(instance: impl Into<String>, out: Box<dyn Write + Send>) -> Self {
            Self {
                instance: instance.into(),
                out: Mutex::new((0, out)),
            }
        }

        pub fn emit(&self, mut event: Value) {
            let Ok(mut guard) = self.out.lock() else {
                return;
            };
            let (seq, out) = &mut *guard;
            *seq += 1;
            event["seq"] = json!(*seq);
            event["ts"] = json!(crate::storage::util::unix_millis());
            event["instance"] = json!(self.instance);
            if let Ok(line) = serde_json::to_string(&event) {
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
    }
}
pub mod modes {

    string_enum! {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum Mode {
            Plan => "plan",
            Build => "build",
            Explore => "explore",
            Retro => "retro",
        }
    }

    impl Mode {
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
    }

    pub const SHARED_PREAMBLE: &str = include_str!("prompts/shared.md");
}
pub mod proc {
    use anyhow::{Context, Result};
    use std::fs::File;
    use std::io::Read;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use crate::storage::util::{TempPath, tmp_file};

    #[derive(Debug)]
    pub struct ProcResult {
        pub code: Option<i32>,
        pub stdout: String,
        pub stderr: String,
        pub timed_out: bool,
        pub duration_ms: u64,
    }

    fn read_capped(path: &Path, cap: usize) -> String {
        let Ok(f) = File::open(path) else {
            return String::new();
        };
        let mut buf = Vec::new();
        let _ = f.take(cap as u64).read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Run `program`, capturing at most `max_read_bytes` of each output stream.
    pub fn run_capture(
        program: &str,
        args: &[String],
        cwd: &Path,
        tmpdir: &Path,
        timeout: Duration,
        max_read_bytes: usize,
    ) -> Result<ProcResult> {
        std::fs::create_dir_all(tmpdir).ok();
        let out = TempPath(tmp_file(tmpdir, ".out", "tmp"));
        let err = TempPath(tmp_file(tmpdir, ".err", "tmp"));
        let out_file = File::create(&out.0).context("creating stdout temp")?;
        let err_file = File::create(&err.0).context("creating stderr temp")?;

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
            std::thread::sleep(Duration::from_millis(25));
        };

        Ok(ProcResult {
            code,
            stdout: read_capped(&out.0, max_read_bytes),
            stderr: read_capped(&err.0, max_read_bytes),
            timed_out,
            duration_ms: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    }
}
pub mod registry {
    //! A lightweight registry of running genji instances.

    use anyhow::{Context, Result, bail};
    use serde::{Deserialize, Serialize};
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use crate::storage::util::unix_secs;

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
        fn path(&self) -> PathBuf {
            dir().join(format!("{}.json", self.id))
        }

        pub fn save(&self) -> Result<()> {
            let d = dir();
            std::fs::create_dir_all(&d)
                .with_context(|| format!("creating instance registry {}", d.display()))?;
            // Write then rename so readers never see (and delete) a partial record.
            let p = self.path();
            let tmp = p.with_extension("json.tmp");
            let text = serde_json::to_string_pretty(self)?;
            std::fs::write(&tmp, format!("{text}\n"))
                .with_context(|| format!("writing instance record {}", tmp.display()))?;
            std::fs::rename(&tmp, &p)
                .with_context(|| format!("writing instance record {}", p.display()))
        }

        pub fn uptime_secs(&self) -> u64 {
            unix_secs().saturating_sub(self.started_at)
        }

        /// The live status line, or `None` when the instance does not answer.
        pub fn status(&self) -> Option<String> {
            let reply = crate::socket::send(Path::new(&self.control_socket), "/status").ok()?;
            (!reply.is_empty()).then(|| {
                reply
                    .strip_prefix("status:")
                    .unwrap_or(&reply)
                    .trim()
                    .to_string()
            })
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

    /// A short, human-friendly instance id, unique among currently-registered ids.
    pub fn new_id() -> String {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX));
        let mut x =
            (nanos ^ (u64::from(std::process::id()) << 21)).wrapping_mul(0x9e37_79b9_7f4a_7c15);
        x ^= x >> 32;
        let d = dir();
        loop {
            let id = format!("{:06x}", x & 0x00ff_ffff);
            if !d.join(format!("{id}.json")).exists() {
                return id;
            }
            x = x.wrapping_add(1);
        }
    }

    /// Every registry record, oldest first, without checking liveness.
    /// Unreadable records are removed.
    fn records() -> Vec<Instance> {
        let Ok(rd) = std::fs::read_dir(dir()) else {
            return Vec::new();
        };
        let mut out: Vec<Instance> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
            .filter_map(|p| {
                let inst = std::fs::read_to_string(&p)
                    .ok()
                    .and_then(|t| serde_json::from_str(&t).ok());
                if inst.is_none() {
                    let _ = std::fs::remove_file(&p);
                }
                inst
            })
            .collect();
        out.sort_by(|a, b| (a.started_at, &a.id).cmp(&(b.started_at, &b.id)));
        out
    }

    /// The instance with its live status, or `None` (and its record removed)
    /// when it does not answer.
    fn live(inst: Instance) -> Option<(Instance, String)> {
        let status = inst.status();
        if status.is_none() {
            remove(&inst.id);
        }
        status.map(|s| (inst, s))
    }

    /// Running instances with their live status. Stale records are removed.
    pub fn list_live() -> Vec<(Instance, String)> {
        records().into_iter().filter_map(live).collect()
    }

    /// The running instance whose id equals or uniquely starts with `id`.
    /// Only matching records are probed.
    pub fn find(id: &str) -> Result<Instance> {
        let id = id.trim();
        if id.is_empty() {
            bail!("missing instance id (see `genji list`)");
        }
        let mut matches: Vec<Instance> = records()
            .into_iter()
            .filter(|i| i.id.starts_with(id))
            .filter_map(|i| live(i).map(|(i, _)| i))
            .collect();
        if let Some(i) = matches.iter().position(|i| i.id == id) {
            return Ok(matches.swap_remove(i));
        }
        match matches.len() {
            1 => Ok(matches.remove(0)),
            0 => bail!("no running genji instance with id `{id}` (see `genji list`)"),
            _ => {
                let ids: Vec<&str> = matches.iter().map(|i| i.id.as_str()).collect();
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

    use anyhow::{Context, Result};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Write `text` to `path`, creating missing parent directories.
    pub fn write_file(path: &Path, text: impl AsRef<[u8]>) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
    }

    /// A unique path under `dir` for a scratch file named `<prefix>-<pid>-<n>.<ext>`.
    pub fn tmp_file(dir: &Path, prefix: &str, ext: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("{prefix}-{}-{n}.{ext}", std::process::id()))
    }

    /// A scratch file path removed on drop.
    pub struct TempPath(pub PathBuf);

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

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
    #[derive(Debug, Clone, Default)]
    pub enum FieldPatch<T> {
        #[default]
        Keep,
        Set(T),
        Clear,
    }

    #[cfg(feature = "formal")]
    impl<'de, T: serde::de::DeserializeOwned> serde::Deserialize<'de> for FieldPatch<T> {
        fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            Ok(match Option::<T>::deserialize(deserializer)? {
                Some(value) => Self::Set(value),
                None => Self::Clear,
            })
        }
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

    /// Load every markdown record under `dir`, assigning ids to files that lack
    /// one (and writing the id back).
    #[cfg(feature = "formal")]
    pub fn load_records<T>(
        dir: &Path,
        build: impl Fn(MarkdownDocument, i64) -> T,
        render: impl Fn(&T) -> String,
    ) -> Result<Vec<T>> {
        let raws = load_markdown_dir(dir)?;
        let mut next = next_id(&raws, |raw| meta_i64(&raw.meta, "id").unwrap_or(0));
        raws.into_iter()
            .map(|raw| {
                let (id, fresh_path) = match meta_i64(&raw.meta, "id") {
                    Some(id) => (id, None),
                    None => {
                        let id = next;
                        next = next.saturating_add(1);
                        (id, Some(raw.path.clone()))
                    }
                };
                let record = build(raw, id);
                if let Some(path) = fresh_path {
                    write_file(&path, render(&record))?;
                }
                Ok(record)
            })
            .collect()
    }

    #[cfg(feature = "formal")]
    pub fn document_title(doc: &MarkdownDocument, fallback: &str) -> String {
        nonempty_meta(&doc.meta, "title")
            .or_else(|| doc.heading.clone())
            .or_else(|| {
                doc.path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| fallback.to_owned())
    }

    #[cfg(feature = "formal")]
    pub fn next_id<T>(items: &[T], id: impl Fn(&T) -> i64) -> i64 {
        items.iter().map(id).max().unwrap_or(0).saturating_add(1)
    }

    #[cfg(feature = "formal")]
    pub fn timestamps(meta: &BTreeMap<String, String>) -> (String, String) {
        let created = nonempty_meta(meta, "created").unwrap_or_else(|| unix_secs().to_string());
        let updated = nonempty_meta(meta, "updated").unwrap_or_else(|| created.clone());
        (created, updated)
    }

    #[cfg(feature = "formal")]
    pub fn persist_renamed(old_path: &Path, new_path: &Path, text: &str) -> Result<()> {
        write_file(new_path, text)?;
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

    pub fn split_frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
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

    /// A bare file-name slug: letters, digits, `-` and `_`, at most 64 chars.
    pub fn valid_slug(slug: &str) -> bool {
        !slug.is_empty()
            && slug.len() <= 64
            && slug
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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

    /// A fresh, empty scratch directory for a test.
    #[cfg(test)]
    pub fn temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("genji-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(test)]
    mod tests {
        use super::{resolve_path, slugify, valid_slug};
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
        fn slugs_are_safe_file_names() {
            assert!(valid_slug("rate-limiting"));
            assert!(valid_slug("Plan_2"));
            assert!(!valid_slug(""));
            assert!(!valid_slug("../escape"));
            assert!(!valid_slug("a/b"));
            assert!(!valid_slug("has space"));
            assert!(!valid_slug(&"x".repeat(65)));
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

        #[cfg(feature = "formal")]
        #[test]
        fn heading_extraction() {
            let titled = super::parse_markdown("test.md".into(), "intro\n# Real Title\nbody");
            assert_eq!(titled.heading, Some("Real Title".into()));
            assert_eq!(titled.body, "intro\nbody");
            let plain = super::parse_markdown("test.md".into(), "no heading");
            assert_eq!(plain.heading, None);
            assert_eq!(plain.body, "no heading");
        }

        #[cfg(feature = "formal")]
        #[test]
        fn frontmatter_parsing() {
            let doc = super::parse_markdown(
                "test.md".into(),
                "---\nid: 4\nlevel: system\n---\n# T\nbody\n",
            );
            assert_eq!(doc.meta.get("id").map(String::as_str), Some("4"));
            assert_eq!(doc.meta.get("level").map(String::as_str), Some("system"));
            assert_eq!(doc.heading, Some("T".into()));
            assert_eq!(doc.body, "body");
        }
    }
}
#[cfg(feature = "formal")]
pub mod reqmd {
    //! File-backed requirement store.

    use anyhow::{Context, Result};
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};

    use crate::config::Config;
    use crate::storage::util::{
        document_title, load_records, meta_i64, next_id, nonempty_meta, persist_renamed,
        relative_path, slugify, timestamps, write_file,
    };

    string_enum! {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub enum RequirementLevel {
            #[default]
            Stakeholder => "stakeholder",
            System => "system",
        }
    }

    string_enum! {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub enum RequirementStatus {
            #[default]
            Active => "active",
            Met => "met",
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

    fn path_for(cfg: &Config, workspace: &Path, r: &Requirement) -> PathBuf {
        cfg.requirements_path(workspace).join(format!(
            "{}-{}.md",
            r.id,
            slugify(&r.title, "requirement")
        ))
    }

    pub fn load_all(cfg: &Config, workspace: &Path) -> Result<Vec<Requirement>> {
        let mut out = load_records(
            &cfg.requirements_path(workspace),
            |raw, id| {
                let level = nonempty_meta(&raw.meta, "level")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_default();
                let title = document_title(&raw, "requirement");
                let status = nonempty_meta(&raw.meta, "status")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_default();
                let parent_id = meta_i64(&raw.meta, "parent");
                let source = nonempty_meta(&raw.meta, "source").unwrap_or_else(|| "user_md".into());
                let (created_at, updated_at) = timestamps(&raw.meta);
                Requirement {
                    id,
                    level,
                    title,
                    body: raw.body,
                    status,
                    parent_id,
                    source,
                    path: raw.path,
                    created_at,
                    updated_at,
                }
            },
            render,
        )?;
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
        let id = next_id(&load_all(cfg, workspace)?, |r| r.id);
        let now = crate::storage::util::unix_secs().to_string();
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
        write_file(&path, render(&req))?;
        Ok(req)
    }

    /// Apply `edit` to requirement `id`, then persist it (renaming its file when
    /// the title changed). `None` when the requirement does not exist.
    pub fn update(
        cfg: &Config,
        workspace: &Path,
        id: i64,
        edit: impl FnOnce(&mut Requirement),
    ) -> Result<Option<Requirement>> {
        let Some(mut req) = load_by_id(cfg, workspace, id)? else {
            return Ok(None);
        };
        edit(&mut req);
        req.updated_at = crate::storage::util::unix_secs().to_string();
        let new_path = path_for(cfg, workspace, &req);
        let old_path = std::mem::replace(&mut req.path, new_path);
        persist_renamed(&old_path, &req.path, &render(&req))?;
        Ok(Some(req))
    }

    pub fn remove(cfg: &Config, workspace: &Path, id: i64) -> Result<bool> {
        let Some(req) = load_by_id(cfg, workspace, id)? else {
            return Ok(false);
        };
        std::fs::remove_file(&req.path)
            .with_context(|| format!("deleting {}", req.path.display()))?;
        Ok(true)
    }

    #[cfg(test)]
    mod tests {
        use super::{
            RequirementLevel, RequirementStatus, active_count, create, load_all, load_by_id,
            remove, update,
        };
        use crate::config::Config;
        use crate::storage::util::temp_dir as temp_workspace;

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
                update(&cfg, &ws, 2, |r| {
                    r.title = "Accept Files and URLs".into();
                    r.status = RequirementStatus::Met;
                })
                .unwrap()
                .is_some()
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

            update(&cfg, &ws, 2, |r| r.level = RequirementLevel::Stakeholder).unwrap();
            let r2c = load_by_id(&cfg, &ws, 2).unwrap().unwrap();
            assert_eq!(r2c.level, RequirementLevel::Stakeholder);
            assert!(
                r2c.display_path(&ws)
                    .ends_with(".genji/requirements/2-accept-files-and-urls.md")
            );

            assert!(remove(&cfg, &ws, 2).unwrap());
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
    use std::fmt::Write as _;
    use std::path::{Path, PathBuf};

    use crate::config::Config;
    use crate::storage::util::{
        document_title, load_records, meta_i64, next_id, nonempty_meta, persist_renamed,
        relative_path, slugify, timestamps, write_file,
    };

    string_enum! {
        #[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
        pub enum TicketStatus {
            #[default]
            Open => "open",
            InProgress => "in_progress",
            Resolved => "resolved",
            Closed => "closed",
        }
    }

    impl TicketStatus {
        pub const fn is_open(self) -> bool {
            matches!(self, Self::Open | Self::InProgress)
        }

        pub const fn is_done(self) -> bool {
            matches!(self, Self::Resolved | Self::Closed)
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

    pub fn path_for(cfg: &Config, workspace: &Path, t: &Ticket) -> PathBuf {
        cfg.tickets_path(workspace)
            .join(format!("{}-{}.md", t.id, slugify(&t.title, "ticket")))
    }

    pub fn load_all(cfg: &Config, workspace: &Path) -> Result<Vec<Ticket>> {
        let mut out = load_records(
            &cfg.tickets_path(workspace),
            |raw, id| {
                let title = document_title(&raw, "ticket");
                let status = nonempty_meta(&raw.meta, "status")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_default();
                let priority = meta_i64(&raw.meta, "priority").unwrap_or(2).clamp(1, 3);
                let parent_id = meta_i64(&raw.meta, "parent");
                let requirement_id = meta_i64(&raw.meta, "requirement");
                let mode = nonempty_meta(&raw.meta, "mode");
                let resolution = nonempty_meta(&raw.meta, "resolution");
                let (created_at, updated_at) = timestamps(&raw.meta);
                Ticket {
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
                    path: raw.path,
                }
            },
            render,
        )?;
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
        let id = next_id(&load_all(cfg, workspace)?, |ticket| ticket.id);
        let now = crate::storage::util::unix_secs().to_string();
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
        write_file(&path, render(&ticket))?;
        Ok(ticket)
    }

    /// Apply `edit` to ticket `id`, then persist it (renaming its file when the
    /// title changed). `None` when the ticket does not exist.
    pub fn update(
        cfg: &Config,
        workspace: &Path,
        id: i64,
        edit: impl FnOnce(&mut Ticket),
    ) -> Result<Option<Ticket>> {
        let Some(mut ticket) = load_by_id(cfg, workspace, id)? else {
            return Ok(None);
        };
        edit(&mut ticket);
        ticket.priority = ticket.priority.clamp(1, 3);
        ticket.updated_at = crate::storage::util::unix_secs().to_string();
        let new_path = path_for(cfg, workspace, &ticket);
        let old_path = std::mem::replace(&mut ticket.path, new_path);
        persist_renamed(&old_path, &ticket.path, &render(&ticket))?;
        Ok(Some(ticket))
    }
}
