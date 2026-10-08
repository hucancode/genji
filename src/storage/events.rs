//! Machine-readable event stream: JSON lines on stdout and, with the full
//! context operations, in `.genji/sessions/<id>.jsonl`. The session file is
//! the durable log; `replay` folds it back into the exact context.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::context::ContextComposer;
use super::util;
use crate::llm::{ChatMessage, Role, ToolCall};
use crate::socket::Control;

struct Sink {
    seq: u64,
    stdout: Box<dyn Write + Send>,
    file: Option<File>,
}

pub struct EventEmitter {
    instance: String,
    out: Mutex<Sink>,
    /// Control socket whose watchers get each event rendered as text.
    tap: Option<Arc<Control>>,
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

    fn build(instance: &str, stdout: Box<dyn Write + Send>, file: Option<File>, seq: u64) -> Self {
        Self {
            instance: instance.to_string(),
            out: Mutex::new(Sink { seq, stdout, file }),
            tap: None,
        }
    }

    /// Also show every event to the watchers of `control`.
    pub fn with_tap(mut self, control: Arc<Control>) -> Self {
        self.tap = Some(control);
        self
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
        #[cfg(feature = "socket")]
        if let Some(t) = &self.tap
            && let Some(text) = render(&event)
        {
            t.publish(&text);
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
            "control_socket": self.tap.as_ref().and_then(|c| c.path.as_ref()).map(|p| p.display().to_string()),
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

    pub fn prune(&self, keep: usize, bulk: bool) {
        self.emit(json!({ "type": "prune", "keep": keep, "bulk": bulk }));
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

#[cfg(feature = "socket")]
/// An event as a short human-readable block for `/watch`; `None` for events a person
/// has no use for (token counts, status ticks, the system prompt).
pub fn render(e: &Value) -> Option<String> {
    let s = |k: &str| e[k].as_str().unwrap_or_default();
    let clip = |t: &str, n: usize| crate::storage::util::truncate(t.trim(), n).replace('\n', " ");
    Some(match e["type"].as_str()? {
        "instance_start" => format!(
            "== {} started ({}): {}",
            s("agent"),
            s("model"),
            clip(s("task"), 200)
        ),
        "instance_end" => match e["reason"].as_str() {
            Some(r) => format!("== ended: {} ({r})", s("status")),
            None => format!("== ended: {}", s("status")),
        },
        "user" => format!("> {}", s("content").trim()),
        "assistant" => {
            let text = s("content").trim();
            if text.is_empty() {
                return None;
            }
            text.to_string()
        }
        "tool_call" if e["name"] == "ask" => {
            let a = &e["arguments"];
            let options: Vec<&str> = a["options"]
                .as_array()
                .map(|o| o.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            format!(
                "? {}\n  options: {}\n  recommended: {}\n  reply with: /answer {} <text>",
                a["question"].as_str().unwrap_or_default(),
                options.join(" | "),
                a["recommended"].as_str().unwrap_or_default(),
                s("id")
            )
        }
        "tool_call" => {
            let args = match e.get("arguments") {
                Some(a) => a.to_string(),
                None => s("raw_arguments").to_string(),
            };
            format!("-> {}({})", s("name"), clip(&args, 120))
        }
        "tool_result" => {
            let size = s("result").len();
            if e["is_error"] == true {
                format!("<- {} ERROR: {}", s("name"), clip(s("result"), 160))
            } else {
                format!("<- {} ok {}ms ({size} bytes)", s("name"), e["duration_ms"])
            }
        }
        "compaction" => format!("~ summarized {} messages", e["removed"]),
        "prune" => "~ pruned old output".to_string(),
        "error" => format!("! {}", s("message")),
        _ => return None,
    })
}

/// The state a run starts from: fresh, or a session folded back into the state the run had
/// when it stopped.
pub struct Replayed {
    pub model: String,
    /// `None` for a fresh run.
    pub ctx: Option<ContextComposer>,
    pub seq: u64,
    pub tokens_used: i64,
    /// Calls of the last assistant turn that have no result.
    pub pending: Vec<ToolCall>,
}

impl Replayed {
    pub fn fresh(model: String) -> Self {
        Self {
            model,
            ctx: None,
            seq: 0,
            tokens_used: 0,
            pending: Vec::new(),
        }
    }
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

/// The JSON events of `text` (one per line); unparseable lines (a write cut off by a
/// crash) are dropped.
pub fn parse_lines(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The events recorded in a session file.
pub fn read(path: &Path) -> Result<Vec<Value>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(parse_lines(&text))
}

/// Fold the log through the same context operations the run applied.
/// Unparseable lines (a write cut off by a crash) are dropped.
pub fn replay(path: &Path, context_window: i64) -> Result<Replayed> {
    let mut ctx: Option<ContextComposer> = None;
    let (mut model, mut seq, mut tokens_used): (String, u64, i64) = (String::new(), 0, 0);
    let text_of = |e: &Value, k: &str| e[k].as_str().unwrap_or_default().to_string();
    for e in read(path)? {
        seq = seq.max(e["seq"].as_u64().unwrap_or(0));
        match (e["type"].as_str().unwrap_or_default(), ctx.as_mut()) {
            ("instance_start", _) => {
                model = text_of(&e, "model");
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
                c.prune(
                    e["keep"].as_u64().unwrap_or(0) as usize,
                    e["bulk"].as_bool().unwrap_or(true),
                );
            }
            ("compaction", Some(c)) => {
                c.apply_compaction(
                    &text_of(&e, "summary"),
                    e["kept"].as_u64().unwrap_or(0) as usize,
                );
                tokens_used = tokens_used.max(e["used"].as_i64().unwrap_or(0));
            }
            ("tokens", Some(c)) => {
                tokens_used = tokens_used.max(e["used"].as_i64().unwrap_or(0));
                c.set_last_prompt_tokens(e["prompt"].as_i64().unwrap_or(0));
            }
            _ => {}
        }
    }
    let Some(ctx) = ctx else {
        bail!("{} has no recorded context", path.display());
    };
    let pending = unanswered(&ctx.messages()[1..]);
    Ok(Replayed {
        model,
        ctx: Some(ctx),
        seq,
        tokens_used,
        pending,
    })
}

/// Opens a user message carrying an instruction queued over the control socket.
pub const INSTRUCTION: &str = "[instruction from user]\n";
/// Opens a user message carrying a review pass's findings.
pub const REJECTED: &str = "[review rejected]\n";

/// What a review pass judges, read from the work instance's session so compaction and
/// resumes lose nothing: the request, the user's instructions, the `ask` decisions,
/// earlier review findings, and the work pass's last report.
pub fn review_input(path: &Path) -> Result<String> {
    let events = read(path)?;
    let str_of = |e: &Value, k: &str| e[k].as_str().unwrap_or_default().to_string();
    // The first start carries the request; a resume with a task is a follow-up request.
    let mut starts = events.iter().filter(|e| e["type"] == "instance_start");
    let request = starts.next().map(|e| str_of(e, "task")).unwrap_or_default();
    let follow_ups: Vec<String> = starts
        .map(|e| str_of(e, "task"))
        .filter(|t| !t.trim().is_empty() && !t.starts_with(REJECTED))
        .collect();
    let mut instructions = Vec::new();
    let mut findings = Vec::new();
    let mut decisions = Vec::new();
    for e in &events {
        match e["type"].as_str().unwrap_or_default() {
            "user" => {
                let c = str_of(e, "content");
                if let Some(i) = c.strip_prefix(INSTRUCTION) {
                    instructions.push(i.to_string());
                } else if let Some(f) = c.strip_prefix(REJECTED) {
                    findings.push(f.to_string());
                }
            }
            "tool_call" if e["name"] == "ask" => {
                let answer = events
                    .iter()
                    .find(|r| r["type"] == "tool_result" && r["id"] == e["id"])
                    .map_or("(no answer)".into(), |r| str_of(r, "result"));
                decisions.push(format!(
                    "Q: {}
A: {answer}",
                    e["arguments"]["question"].as_str().unwrap_or_default()
                ));
            }
            _ => {}
        }
    }
    let report = events
        .iter()
        .rev()
        .find(|e| e["type"] == "instance_end")
        .map(|e| match e["reason"].as_str() {
            Some(r) => format!("(stopped: {r}) {}", str_of(e, "report")),
            None => str_of(e, "report"),
        })
        .unwrap_or_default();
    let mut out = format!("# Request\n{request}\n");
    for (title, items) in [
        ("Follow-up requests", &follow_ups),
        ("User instructions given while working", &instructions),
        ("Decisions made with the human", &decisions),
        ("Earlier review findings", &findings),
    ] {
        if !items.is_empty() {
            out.push_str(&format!("\n# {title}\n"));
            for i in items {
                out.push_str(&format!("- {}\n", i.trim().replace('\n', "\n  ")));
            }
        }
    }
    out.push_str(&format!("\n# Work pass report\n{report}\n"));
    Ok(out)
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

/// The newest session in `dir` that can be resumed on its own: a top-level instance, not a
/// subagent or a review pass.
pub fn last_session(dir: &Path) -> Result<PathBuf> {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .filter(|p| {
            start(p).is_ok_and(|s| {
                s["depth"] == 0 && !s["agent"].as_str().unwrap_or_default().ends_with(":review")
            })
        })
        .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
        .max()
        .map(|(_, p)| p)
        .with_context(|| format!("no recorded instance in {}", dir.display()))
}

/// The `instance_start` event of a recorded instance: its id (`instance`), agent and parent.
pub fn start(path: &Path) -> Result<Value> {
    read(path)?
        .into_iter()
        .find(|e| e["type"] == "instance_start")
        .with_context(|| format!("{} has no instance_start", path.display()))
}

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
        assert!(live.prune(2, true));
        e.prune(2, true);
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
        let ctx = r.ctx.unwrap();
        assert_eq!(
            serde_json::to_string(ctx.messages()).unwrap(),
            serde_json::to_string(live.messages()).unwrap()
        );
        assert_eq!(ctx.tools(), live.tools());
        assert_eq!((r.model.as_str(), r.tokens_used), ("m", 700));
        assert_eq!(ctx.est_tokens(), live.est_tokens());
        assert_eq!(start(&path).unwrap()["agent"], "build");
        assert_eq!(find_session(&dir, "s").unwrap(), path);
    }

    #[test]
    fn last_session_is_the_newest_top_level_instance() {
        let dir = util::temp_dir("last-session");
        let open = |id: &str, agent: &str, depth: u32| {
            let e = EventEmitter::open(id, &dir.join(format!("{id}.jsonl")), 0).unwrap();
            e.instance_start("/w", agent, "m", None, depth, "t", false);
        };
        assert!(last_session(&dir).is_err());
        open("a", "build", 0);
        std::thread::sleep(std::time::Duration::from_millis(20));
        open("b", "explore", 1);
        std::thread::sleep(std::time::Duration::from_millis(20));
        open("c", "build:review", 0);
        assert_eq!(last_session(&dir).unwrap(), dir.join("a.jsonl"));
        std::thread::sleep(std::time::Duration::from_millis(20));
        open("d", "plan", 0);
        assert_eq!(last_session(&dir).unwrap(), dir.join("d.jsonl"));
    }

    #[test]
    fn review_input_collects_request_instructions_decisions_and_report() {
        let dir = util::temp_dir("review-input");
        let path = dir.join("w.jsonl");
        let e = EventEmitter::open("w", &path, 0).unwrap();
        e.instance_start("/w", "build", "m", None, 0, "add a flag", false);
        e.system("sys", &[]);
        e.user("add a flag");
        e.user(&format!("{INSTRUCTION}name it --fast"));
        e.tool_call(&ToolCall::new("a1", "ask", r#"{"question":"default on?"}"#));
        e.tool_result("a1", "ask", false, 1, "answer: no");
        e.compaction("SUMMARY", 1, 3, 10);
        e.instance_end("done", None, 10, "first try", None);
        e.instance_start("/w", "build", "m", None, 0, "fix", true);
        e.user(&format!("{REJECTED}missing test"));
        e.instance_end("stopped", Some("max_iterations"), 20, "added test", None);
        drop(e);
        let out = review_input(&path).unwrap();
        assert!(out.starts_with("# Request\nadd a flag\n"), "{out}");
        assert!(out.contains("# Follow-up requests\n- fix\n"), "{out}");
        assert!(out.contains("- name it --fast"), "{out}");
        assert!(out.contains("- Q: default on?\n  A: answer: no"), "{out}");
        assert!(
            out.contains("# Earlier review findings\n- missing test"),
            "{out}"
        );
        assert!(
            out.ends_with("# Work pass report\n(stopped: max_iterations) added test\n"),
            "{out}"
        );
    }

    #[cfg(feature = "socket")]
    #[test]
    fn render_is_readable_and_skips_noise() {
        let r = |e: Value| render(&e);
        assert_eq!(
            r(json!({"type":"user","content":" hi "})).as_deref(),
            Some("> hi")
        );
        assert_eq!(r(json!({"type":"assistant","content":" "})), None);
        assert_eq!(r(json!({"type":"tokens","used":1})), None);
        assert_eq!(r(json!({"type":"status","status":"x"})), None);
        assert_eq!(
            r(json!({"type":"tool_call","id":"a","name":"read","arguments":{"path":"x"}}))
                .as_deref(),
            Some(r#"-> read({"path":"x"})"#)
        );
        let ok = r(json!({"type":"tool_result","name":"bash","is_error":false,"duration_ms":12,"result":"abc"})).unwrap();
        assert_eq!(ok, "<- bash ok 12ms (3 bytes)");
        let err =
            r(json!({"type":"tool_result","name":"bash","is_error":true,"result":"boom\nmore"}))
                .unwrap();
        assert_eq!(err, "<- bash ERROR: boom more");
        let ask = r(
            json!({"type":"tool_call","id":"c7","name":"ask","arguments":
            {"question":"db?","options":["pg","sqlite"],"recommended":"pg"}}),
        )
        .unwrap();
        assert!(
            ask.contains("? db?")
                && ask.contains("pg | sqlite")
                && ask.contains("/answer c7 <text>"),
            "{ask}"
        );
        assert_eq!(
            r(json!({"type":"instance_end","status":"stopped","reason":"user"})).as_deref(),
            Some("== ended: stopped (user)")
        );
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
