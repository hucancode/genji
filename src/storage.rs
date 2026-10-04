pub mod context {
    use serde_json::{Value, json};
    use std::fmt::Write as _;

    use crate::llm::{self, ChatMessage, Role};

    /// The prompt sent to the model: system message, tools and turns. Every
    /// mutation is deterministic so replaying the session log rebuilds it exactly.
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

        pub fn set_system(&mut self, system: String, tools: Vec<Value>) {
            self.messages[0].content = system;
            self.tools = tools;
        }

        pub fn push(&mut self, msg: ChatMessage) {
            self.messages.push(msg);
        }

        pub fn set_last_prompt_tokens(&mut self, tokens: i64) {
            self.last_prompt_tokens = tokens;
            self.prompt_len = self.messages.len();
        }

        /// The last measured prompt size plus an estimate for messages added since.
        pub fn est_tokens(&self) -> i64 {
            if self.last_prompt_tokens > 0 {
                self.last_prompt_tokens + llm::estimate_messages(&self.messages[self.prompt_len..])
            } else {
                llm::estimate_messages(&self.messages)
            }
        }

        pub fn over(&self, fraction: f64) -> bool {
            self.est_tokens() >= (self.context_window as f64 * fraction) as i64
        }

        pub fn snapshot(&self) -> Value {
            json!({
                "context_window": self.context_window,
                "last_prompt_tokens": self.last_prompt_tokens,
                "messages": self.messages,
                "tools": self.tools,
            })
        }

        /// Elide bulky tool results older than the last `keep` messages.
        pub fn prune(&mut self, keep: usize) -> bool {
            const MAX: usize = 1000;
            let old = self.messages.len().saturating_sub(keep.max(2));
            let mut changed = false;
            for m in &mut self.messages[..old] {
                if m.role == Role::Tool && m.content.len() > MAX {
                    m.content = format!(
                        "{}\n[older output elided: {} bytes; re-run the tool if needed]",
                        llm::truncate(&m.content, MAX / 2),
                        m.content.len()
                    );
                    changed = true;
                }
            }
            if changed {
                self.last_prompt_tokens = 0;
            }
            changed
        }

        /// The text to summarize and how many trailing messages stay verbatim
        /// (tool results never lose their call).
        pub fn compaction_source(&self, keep: usize) -> Option<(String, usize)> {
            let keep = keep.max(2);
            let len = self.messages.len();
            if len <= keep + 2 {
                return None;
            }
            let mut split = len - keep;
            while split < len && self.messages[split].role == Role::Tool {
                split += 1;
            }
            Some((render(&self.messages[1..split]), len - split))
        }

        pub fn apply_compaction(&mut self, summary: &str, kept: usize) {
            let recent = self.messages.split_off(self.messages.len() - kept);
            self.messages.truncate(1);
            self.messages.push(ChatMessage::user(format!(
                "[compacted summary of earlier conversation]\n{summary}"
            )));
            self.messages.extend(recent);
            self.last_prompt_tokens = 0;
        }
    }

    fn render(msgs: &[ChatMessage]) -> String {
        let mut out = String::new();
        for m in msgs {
            let _ = write!(out, "[{:?}] {}", m.role, m.content);
            if !m.tool_calls.is_empty() {
                let calls: Vec<String> = m
                    .tool_calls
                    .iter()
                    .map(|c| format!("{}({})", c.name(), llm::truncate(c.args(), 200)))
                    .collect();
                let _ = write!(out, "\n  calls: {}", calls.join(", "));
            }
            out.push_str("\n\n");
        }
        llm::truncate(&out, 120_000).into_owned()
    }

    #[cfg(test)]
    mod tests {
        use super::ContextComposer;
        use crate::llm::ChatMessage;

        #[test]
        fn estimate_counts_messages_after_last_prompt() {
            let mut ctx = ContextComposer::new("sys".to_string(), Vec::new(), 1000);
            ctx.set_last_prompt_tokens(100);
            assert_eq!(ctx.est_tokens(), 108);
            ctx.push(ChatMessage::tool_result("c1", "x".repeat(4000)));
            assert_eq!(ctx.est_tokens(), 100 + 1004 + 8);
        }

        #[test]
        fn prunes_only_old_bulky_tool_results() {
            let mut c = ContextComposer::new("sys".into(), vec![], 100);
            c.push(ChatMessage::tool_result("1", "x".repeat(5000)));
            c.push(ChatMessage::user("mid"));
            c.push(ChatMessage::tool_result("2", "y".repeat(5000)));
            assert!(c.prune(2));
            assert!(c.messages()[1].content.len() < 1000);
            assert_eq!(c.messages()[3].content.len(), 5000);
            assert!(!c.prune(2));
        }

        #[test]
        fn compaction_keeps_tool_results_with_their_calls() {
            let mut c = ContextComposer::new("sys".into(), vec![], 100);
            for i in 0..6 {
                c.push(ChatMessage::user(format!("u{i}")));
            }
            c.push(ChatMessage::tool_result("1", "r"));
            let (text, kept) = c.compaction_source(2).unwrap();
            assert!(text.contains("u0") && !text.contains("[Tool]"));
            c.apply_compaction("S", kept);
            assert_eq!(c.messages()[0].content, "sys");
            assert!(c.messages()[1].content.contains("\nS"));
            assert_eq!(c.messages().len(), 2 + kept);
        }
    }
}

pub mod events {
    //! Machine-readable event stream: JSON lines on stdout and, with the full
    //! context operations, in `.genji/sessions/<id>.jsonl`. The session file is
    //! the durable log; `replay` folds it back into the exact context.

    use anyhow::{Context, Result, bail};
    use serde_json::{Value, json};
    use std::fs::File;
    use std::io::{self, Write};
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::context::ContextComposer;
    use crate::llm::{ChatMessage, Role, ToolCall};

    struct Sink {
        seq: u64,
        stdout: Box<dyn Write + Send>,
        file: Option<File>,
    }

    pub struct EventEmitter {
        instance: String,
        out: Mutex<Sink>,
    }

    impl EventEmitter {
        /// Append to `session`, continuing the numbering after `seq`.
        pub fn open(instance: &str, session: &Path, seq: u64) -> Result<Self> {
            if let Some(dir) = session.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(session)
                .with_context(|| format!("opening {}", session.display()))?;
            Ok(Self::build(
                instance,
                Box::new(io::stdout()),
                Some(file),
                seq,
            ))
        }

        #[cfg(test)]
        pub fn with_writer(instance: &str, out: Box<dyn Write + Send>) -> Self {
            Self::build(instance, out, None, 0)
        }

        fn build(
            instance: &str,
            stdout: Box<dyn Write + Send>,
            file: Option<File>,
            seq: u64,
        ) -> Self {
            Self {
                instance: instance.to_string(),
                out: Mutex::new(Sink { seq, stdout, file }),
            }
        }

        fn write(&self, mut event: Value, to_stdout: bool) {
            let Ok(mut s) = self.out.lock() else { return };
            s.seq += 1;
            event["seq"] = json!(s.seq);
            event["ts"] = json!(util::unix_millis());
            event["instance"] = json!(self.instance);
            let Ok(line) = serde_json::to_string(&event) else {
                return;
            };
            if let Some(f) = &mut s.file {
                let _ = writeln!(f, "{line}");
            }
            if to_stdout {
                let _ = writeln!(s.stdout, "{line}");
                let _ = s.stdout.flush();
            }
        }

        pub fn emit(&self, event: Value) {
            self.write(event, true);
        }

        #[allow(clippy::too_many_arguments)]
        pub fn instance_start(
            &self,
            workspace: &str,
            agent: &str,
            model: &str,
            parent: Option<&str>,
            depth: u32,
            task: &str,
            resumed: bool,
        ) {
            self.emit(json!({
                "type": "instance_start", "resumed": resumed, "workspace": workspace,
                "agent": agent, "model": model, "parent": parent, "depth": depth,
                "task": task, "pid": std::process::id(),
            }));
        }

        /// The full prompt and tools; file only, since stdout consumers have no use for it.
        pub fn system(&self, prompt: &str, tools: &[Value]) {
            self.write(
                json!({ "type": "system", "prompt": prompt, "tools": tools }),
                false,
            );
        }

        pub fn user(&self, content: &str) {
            self.emit(json!({ "type": "user", "content": content }));
        }

        /// The assistant message; its calls keep the raw argument strings.
        pub fn assistant(&self, msg: &ChatMessage) {
            self.emit(json!({
                "type": "assistant", "content": msg.content,
                "reasoning": msg.reasoning_content, "tool_calls": msg.tool_calls,
            }));
        }

        pub fn tool_call(&self, call: &ToolCall) {
            let mut event = json!({ "type": "tool_call", "id": call.id, "name": call.name() });
            match serde_json::from_str::<Value>(call.args()) {
                Ok(args) => event["arguments"] = args,
                Err(_) => event["raw_arguments"] = json!(call.args()),
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
                "type": "tool_result", "id": id, "name": name, "is_error": is_error,
                "duration_ms": duration_ms, "result": result,
            }));
        }

        pub fn tokens(&self, used: i64, prompt: i64, completion: i64, cached: i64) {
            self.emit(json!({ "type": "tokens", "used": used, "prompt": prompt, "completion": completion, "cached": cached }));
        }

        pub fn prune(&self, keep: usize) {
            self.emit(json!({ "type": "prune", "keep": keep }));
        }

        pub fn compaction(&self, summary: &str, kept: usize, removed: usize, used: i64) {
            self.emit(json!({ "type": "compaction", "summary": summary, "kept": kept, "removed": removed, "used": used }));
        }

        pub fn status(&self, status: &str) {
            self.emit(json!({ "type": "status", "status": status }));
        }

        pub fn error(&self, message: &str) {
            self.emit(json!({ "type": "error", "message": message }));
        }

        pub fn instance_end(
            &self,
            status: &str,
            reason: Option<&str>,
            tokens_used: i64,
            report: &str,
            result: Option<&Value>,
        ) {
            self.emit(json!({
                "type": "instance_end", "status": status, "reason": reason,
                "tokens_used": tokens_used, "report": report, "result": result,
            }));
        }
    }

    /// A session folded back into the state the run had when it stopped.
    pub struct Replayed {
        pub id: String,
        pub agent: String,
        pub model: String,
        pub ctx: ContextComposer,
        pub seq: u64,
        pub tokens_used: i64,
        /// The run ended (an `instance_end` is the last lifecycle event).
        pub ended: bool,
        /// Calls of the last assistant turn that have no result.
        pub pending: Vec<ToolCall>,
    }

    /// The calls of the last assistant turn that have no result.
    fn unanswered(messages: &[ChatMessage]) -> Vec<ToolCall> {
        let Some(last) = messages.iter().rposition(|m| !m.tool_calls.is_empty()) else {
            return Vec::new();
        };
        let answered: Vec<&str> = messages[last + 1..]
            .iter()
            .filter(|m| m.role == Role::Tool)
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        messages[last]
            .tool_calls
            .iter()
            .filter(|c| !answered.contains(&c.id.as_str()))
            .cloned()
            .collect()
    }

    /// Fold the log through the same context operations the run applied.
    /// Unparseable lines (a write cut off by a crash) are dropped.
    pub fn replay(path: &Path, context_window: i64) -> Result<Replayed> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let mut ctx: Option<ContextComposer> = None;
        let mut r = Replayed {
            id: String::new(),
            agent: String::new(),
            model: String::new(),
            ctx: ContextComposer::new(String::new(), vec![], context_window),
            seq: 0,
            tokens_used: 0,
            ended: false,
            pending: Vec::new(),
        };
        let text_of = |e: &Value, k: &str| e[k].as_str().unwrap_or_default().to_string();
        for e in text
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        {
            r.seq = r.seq.max(e["seq"].as_u64().unwrap_or(0));
            match (e["type"].as_str().unwrap_or_default(), ctx.as_mut()) {
                ("instance_start", _) => {
                    r.id = text_of(&e, "instance");
                    r.agent = text_of(&e, "agent");
                    r.model = text_of(&e, "model");
                    r.ended = false;
                }
                ("system", c) => {
                    let tools = e["tools"].as_array().cloned().unwrap_or_default();
                    match c {
                        Some(c) => c.set_system(text_of(&e, "prompt"), tools),
                        None => {
                            ctx = Some(ContextComposer::new(
                                text_of(&e, "prompt"),
                                tools,
                                context_window,
                            ))
                        }
                    }
                }
                ("user", Some(c)) => c.push(ChatMessage::user(text_of(&e, "content"))),
                ("assistant", Some(c)) => c.push(ChatMessage {
                    role: Role::Assistant,
                    content: text_of(&e, "content"),
                    tool_calls: serde_json::from_value(e["tool_calls"].clone()).unwrap_or_default(),
                    tool_call_id: None,
                    reasoning_content: e["reasoning"].as_str().map(String::from),
                }),
                ("tool_result", Some(c)) => c.push(ChatMessage::tool_result(
                    &text_of(&e, "id"),
                    text_of(&e, "result"),
                )),
                ("prune", Some(c)) => {
                    c.prune(e["keep"].as_u64().unwrap_or(0) as usize);
                }
                ("compaction", Some(c)) => {
                    c.apply_compaction(
                        &text_of(&e, "summary"),
                        e["kept"].as_u64().unwrap_or(0) as usize,
                    );
                    r.tokens_used = r.tokens_used.max(e["used"].as_i64().unwrap_or(0));
                }
                ("tokens", Some(c)) => {
                    r.tokens_used = r.tokens_used.max(e["used"].as_i64().unwrap_or(0));
                    c.set_last_prompt_tokens(e["prompt"].as_i64().unwrap_or(0));
                }
                ("instance_end", _) => r.ended = true,
                _ => {}
            }
        }
        let Some(ctx) = ctx else {
            bail!("{} has no recorded context", path.display());
        };
        r.pending = unanswered(&ctx.messages()[1..]);
        r.ctx = ctx;
        Ok(r)
    }

    /// The session file of the instance whose id equals or uniquely starts with `prefix`.
    pub fn find_session(dir: &Path, prefix: &str) -> Result<PathBuf> {
        let mut hits: Vec<PathBuf> = std::fs::read_dir(dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.extension().is_some_and(|e| e == "jsonl")
                    && p.file_stem()
                        .and_then(|s| s.to_str())
                        .is_some_and(|s| s.starts_with(prefix))
            })
            .collect();
        match hits.len() {
            1 => Ok(hits.remove(0)),
            0 => bail!("no recorded instance with id `{prefix}`"),
            _ => {
                if let Some(i) = hits
                    .iter()
                    .position(|p| p.file_stem().is_some_and(|s| s == prefix))
                {
                    return Ok(hits.swap_remove(i));
                }
                bail!("instance id `{prefix}` is ambiguous (use a longer prefix)")
            }
        }
    }

    /// Identity, progress and outcome of a recorded instance.
    pub fn summary(path: &Path) -> Result<Value> {
        let text = std::fs::read_to_string(path)?;
        let events: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let first = |t: &str| events.iter().find(|e| e["type"] == t);
        let last = |t: &str| events.iter().rev().find(|e| e["type"] == t);
        let Some(start) = first("instance_start") else {
            bail!("{} has no instance_start", path.display());
        };
        let end = last("instance_end");
        let messages = events
            .iter()
            .filter(|e| {
                matches!(
                    e["type"].as_str(),
                    Some("user" | "assistant" | "tool_result")
                )
            })
            .count();
        Ok(json!({
            "id": start["instance"], "agent": start["agent"], "model": start["model"],
            "parent": start["parent"], "depth": start["depth"], "task": start["task"],
            "status": end.map_or(json!("running"), |e| e["status"].clone()),
            "tokens_used": last("tokens").map_or(json!(0), |e| e["used"].clone()),
            "messages": messages, "started_at": start["ts"],
            "ended_at": end.map(|e| e["ts"].clone()),
            "report": end.map(|e| e["report"].clone()),
            "result": end.map(|e| e["result"].clone()),
        }))
    }

    use super::util;

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::sync::{Arc, Mutex};

        #[derive(Clone, Default)]
        struct SharedBuf(Arc<Mutex<Vec<u8>>>);

        impl Write for SharedBuf {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        #[test]
        fn events_are_jsonl_with_seq_ts_and_instance() {
            let buf = SharedBuf::default();
            let e = EventEmitter::with_writer("sess-1", Box::new(buf.clone()));
            e.user("hello");
            e.tool_call(&ToolCall::new("call_1", "read", "{\"path\":\"a.txt\"}"));
            e.tool_call(&ToolCall::new("id", "bash", "not json"));
            let data = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
            let out: Vec<Value> = data
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect();
            assert_eq!(
                (
                    out[0]["type"].as_str(),
                    out[0]["seq"].as_u64(),
                    out[0]["instance"].as_str()
                ),
                (Some("user"), Some(1), Some("sess-1"))
            );
            assert_eq!(out[1]["arguments"]["path"], "a.txt");
            assert_eq!(out[2]["raw_arguments"], "not json");
        }

        /// Run scripted context operations live while logging, then replay the file.
        #[test]
        fn replay_rebuilds_the_live_context_after_prune_compaction_and_a_cut_line() {
            let dir = util::temp_dir("replay");
            let path = dir.join("s1.jsonl");
            let e = EventEmitter::open("s1", &path, 0).unwrap();
            let tools = vec![json!({"type": "function", "function": {"name": "read"}})];
            let mut live = ContextComposer::new("sys".into(), tools.clone(), 1000);
            e.instance_start("/w", "build", "m", None, 0, "t", false);
            e.system("sys", &tools);
            let push = |live: &mut ContextComposer, m: ChatMessage| {
                match m.role {
                    Role::User => e.user(&m.content),
                    Role::Assistant => e.assistant(&m),
                    _ => e.tool_result(
                        m.tool_call_id.as_deref().unwrap(),
                        "read",
                        false,
                        1,
                        &m.content,
                    ),
                }
                live.push(m);
            };
            push(&mut live, ChatMessage::user("go"));
            let mut a = ChatMessage {
                role: Role::Assistant,
                ..Default::default()
            };
            a.tool_calls = vec![
                ToolCall::new("c1", "read", "{ \"path\" : \"x\" }"),
                ToolCall::new("c2", "read", "{}"),
            ];
            push(&mut live, a);
            push(&mut live, ChatMessage::tool_result("c1", "x".repeat(3000)));
            for i in 0..6 {
                push(&mut live, ChatMessage::user(format!("u{i}")));
            }
            e.tokens(500, 400, 100, 0);
            live.set_last_prompt_tokens(400);
            assert!(live.prune(2));
            e.prune(2);
            let (_, kept) = live.compaction_source(2).unwrap();
            live.apply_compaction("SUMMARY", kept);
            e.compaction("SUMMARY", kept, 5, 700);
            push(&mut live, ChatMessage::user("after"));
            e.instance_end("done", None, 700, "r", None);
            drop(e);
            let mut text = std::fs::read_to_string(&path).unwrap();
            text.push_str("{\"type\":\"user\",\"content\":\"cut");
            std::fs::write(&path, text).unwrap();

            let r = replay(&path, 1000).unwrap();
            assert_eq!(
                serde_json::to_string(r.ctx.messages()).unwrap(),
                serde_json::to_string(live.messages()).unwrap()
            );
            assert_eq!(r.ctx.tools(), live.tools());
            assert_eq!(
                (r.agent.as_str(), r.tokens_used, r.ended),
                ("build", 700, true)
            );
            assert_eq!(r.ctx.est_tokens(), live.est_tokens());
            assert_eq!(summary(&path).unwrap()["status"], "done");
            assert_eq!(find_session(&dir, "s").unwrap(), path);
        }

        #[test]
        fn replay_reports_unanswered_calls() {
            let dir = util::temp_dir("replay-pending");
            let path = dir.join("p.jsonl");
            let e = EventEmitter::open("p", &path, 0).unwrap();
            e.system("sys", &[]);
            let mut a = ChatMessage {
                role: Role::Assistant,
                ..Default::default()
            };
            a.tool_calls = vec![
                ToolCall::new("c1", "bash", "{}"),
                ToolCall::new("c2", "bash", "{}"),
            ];
            e.assistant(&a);
            e.tool_result("c1", "bash", false, 1, "ok");
            let r = replay(&path, 10).unwrap();
            assert_eq!(r.pending.len(), 1);
            assert_eq!(r.pending[0].id, "c2");
            assert!(replay(&dir.join("none.jsonl"), 10).is_err());
        }
    }
}
pub mod proc {
    use anyhow::{Context, Result};
    use std::fs::File;
    use std::io::{Read, Seek, SeekFrom};
    use std::os::unix::process::CommandExt;
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
    }

    /// Read a file, keeping the head and the tail when it exceeds `cap` bytes:
    /// build and test failures usually sit at the end.
    fn read_capped(path: &Path, cap: usize) -> String {
        let Ok(mut f) = File::open(path) else {
            return String::new();
        };
        let len = f.metadata().map_or(0, |m| m.len());
        let half = (cap / 2) as u64;
        let mut head = Vec::new();
        let mut tail = Vec::new();
        if len <= cap as u64 {
            let _ = f.read_to_end(&mut head);
        } else {
            let _ = (&mut f).take(half).read_to_end(&mut head);
            let _ = f.seek(SeekFrom::Start(len - half));
            let _ = f.read_to_end(&mut tail);
        }
        let mut out = String::from_utf8_lossy(&head).into_owned();
        if !tail.is_empty() {
            out.push_str(&format!("\n… [{} bytes omitted] …\n", len - 2 * half));
            out.push_str(&String::from_utf8_lossy(&tail));
        }
        out
    }

    /// Whether a process with this pid exists.
    pub fn alive(pid: u32) -> bool {
        Command::new("bash")
            .args(["-c", &format!("kill -0 {pid}")])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// SIGKILL the process group led by `pid`.
    pub fn kill_group(pid: u32) {
        let _ = Command::new("bash")
            .args(["-c", &format!("kill -KILL -- -{pid}")])
            .stderr(Stdio::null())
            .status();
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
            .process_group(0)
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
                // Kill the whole group so grandchildren do not outlive the timeout.
                kill_group(child.id());
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
        })
    }
}

#[cfg(test)]
mod proc_tests {
    use super::proc::run_capture;
    use std::time::Duration;

    fn run(script: &str, timeout: u64, cap: usize) -> super::proc::ProcResult {
        let ws = super::util::temp_dir("proc");
        run_capture(
            "bash",
            &["-c".into(), script.into()],
            &ws,
            &ws,
            Duration::from_secs(timeout),
            cap,
        )
        .unwrap()
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let r = run("seq 1 5000", 10, 200);
        assert!(r.stdout.starts_with("1\n2\n"));
        assert!(r.stdout.trim_end().ends_with("5000"));
        assert!(r.stdout.contains("bytes omitted"));
    }

    #[test]
    fn timeout_kills_grandchildren() {
        let r = run("sleep 4242 & wait", 1, 1000);
        assert!(r.timed_out);
        let alive = std::process::Command::new("pgrep")
            .args(["-f", "sleep 4242"])
            .status()
            .unwrap()
            .success();
        assert!(!alive);
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

    pub fn unix_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    pub fn unix_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }

    /// Expand a leading `~/`, then resolve relative paths against `workspace`.
    pub fn resolve_path(workspace: &Path, path: &str) -> PathBuf {
        let expanded = match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
            (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
            _ => PathBuf::from(path),
        };
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

    /// Split `---\nkey: value\n---\nbody` into its keys and body.
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
        let s = out.trim_matches('-');
        if s.is_empty() {
            fallback.to_string()
        } else {
            s.to_string()
        }
    }

    /// A fresh, empty scratch directory for a test.
    #[cfg(test)]
    pub fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("genji-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn resolves_workspace_paths() {
            let ws = Path::new("/workspace");
            assert_eq!(resolve_path(ws, "src/main.rs"), ws.join("src/main.rs"));
            assert_eq!(resolve_path(ws, "/tmp/file"), Path::new("/tmp/file"));
        }

        #[test]
        fn slugs_are_safe_file_names() {
            assert!(valid_slug("rate-limiting") && valid_slug("Plan_2"));
            assert!(
                !valid_slug("")
                    && !valid_slug("../escape")
                    && !valid_slug("a/b")
                    && !valid_slug(&"x".repeat(65))
            );
            assert_eq!(
                slugify("Accept image files & URLs!", "x"),
                "accept-image-files-urls"
            );
            assert_eq!(slugify("!!!", "plan"), "plan");
        }

        #[test]
        fn frontmatter_parsing() {
            let (meta, body) = split_frontmatter("---\ntools: read, ls\nname: \"x\"\n---\nbody\n");
            assert_eq!(meta["tools"], "read, ls");
            assert_eq!(meta["name"], "x");
            assert_eq!(body, "body\n");
            assert_eq!(split_frontmatter("plain").1, "plain");
        }
    }
}
