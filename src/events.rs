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
