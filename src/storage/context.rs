use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt::Write as _;

use super::util::truncate;
use crate::llm::{self, ChatMessage, Role, ToolCall};

/// The prompt sent to the model: system message, tools and turns. Every
/// mutation is deterministic so replaying the session log rebuilds it exactly.
pub struct ContextComposer {
    messages: Vec<ChatMessage>,
    tools: Vec<Value>,
    context_window: i64,
    last_prompt_tokens: i64,
    /// The largest prompt measured so far; compaction and pruning do not lower it.
    peak_prompt_tokens: i64,
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
            peak_prompt_tokens: 0,
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
        self.peak_prompt_tokens = self.peak_prompt_tokens.max(tokens);
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

    /// Whether the largest prompt so far reached `fraction` of the window.
    pub fn peaked_over(&self, fraction: f64) -> bool {
        self.peak_prompt_tokens >= (self.context_window as f64 * fraction) as i64
    }

    pub fn over(&self, fraction: f64) -> bool {
        self.est_tokens() >= (self.context_window as f64 * fraction) as i64
    }

    #[cfg(feature = "socket")]
    pub fn snapshot(&self) -> Value {
        json!({
            "context_window": self.context_window,
            "last_prompt_tokens": self.last_prompt_tokens,
            "messages": self.messages,
            "tools": self.tools,
        })
    }

    /// Drops what the model no longer needs, deterministically from the messages alone
    /// (replay applies the same call); see `plan_prune`.
    pub fn prune(&mut self, keep: usize, bulk: bool) -> bool {
        self.prune_if(keep, bulk, |_, _, _| true)
    }

    /// Like `prune`, but applied only when `worth(freed_tokens, est_tokens, context_window)`
    /// agrees, so the cached prefix is only rewritten when it pays.
    pub fn prune_if(
        &mut self,
        keep: usize,
        bulk: bool,
        worth: impl FnOnce(i64, i64, i64) -> bool,
    ) -> bool {
        let edits = plan_prune(&self.messages, keep, bulk);
        let freed: i64 = edits
            .iter()
            .map(|(i, m)| self.messages[*i].est_tokens() - m.est_tokens())
            .sum();
        if edits.is_empty() || !worth(freed, self.est_tokens(), self.context_window) {
            return false;
        }
        for (i, m) in edits {
            self.messages[i] = m;
        }
        self.last_prompt_tokens = 0;
        true
    }

    /// The content of the tool result answering call `id`, if it is still in context.
    pub fn tool_result(&self, id: &str) -> Option<&str> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(id))
            .map(|m| m.content.as_str())
    }

    pub fn len(&self) -> usize {
        self.messages.len()
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

/// The replacements for what the model no longer needs, by message index:
/// - `read` results for a file that was written or edited later, or read again later;
/// - with `bulk`, tool results older than the last `keep` messages; without it, old results
///   stay verbatim so the agent does not re-read what it already saw;
/// - old `write`/`edit` payloads and reasoning, since the file on disk is the truth.
fn plan_prune(messages: &[ChatMessage], keep: usize, bulk: bool) -> Vec<(usize, ChatMessage)> {
    const MAX: usize = 1000;
    const PAYLOAD: usize = 500;
    const STUB: &str =
        "[superseded: the file changed or was read again later; read it again if needed]";
    let old = messages.len().saturating_sub(keep.max(2));
    // call id -> (call message index, tool, path, read signature)
    let mut calls: HashMap<&str, (usize, &str, String, String)> = HashMap::new();
    let mut last_change: HashMap<String, usize> = HashMap::new();
    let mut last_read: HashMap<String, usize> = HashMap::new();
    for (i, m) in messages.iter().enumerate() {
        for c in &m.tool_calls {
            let args: Value = serde_json::from_str(c.args()).unwrap_or(Value::Null);
            let path = args["path"].as_str().unwrap_or_default().to_string();
            let sig = format!("{path}:{}:{}", args["offset"], args["limit"]);
            match c.name() {
                "write" | "edit" if !path.is_empty() => {
                    last_change.insert(path.clone(), i);
                }
                "read" if !path.is_empty() => {
                    last_read.insert(sig.clone(), i);
                }
                _ => {}
            }
            calls.insert(&c.id, (i, c.name(), path, sig));
        }
    }
    let bulky_payload =
        |c: &ToolCall| matches!(c.name(), "write" | "edit") && c.args().len() > PAYLOAD;
    let mut edits = Vec::new();
    for (i, m) in messages.iter().enumerate() {
        let content = if m.role == Role::Tool {
            let stale = m
                .tool_call_id
                .as_deref()
                .and_then(|id| calls.get(id))
                .is_some_and(|(at, name, path, sig)| {
                    *name == "read"
                        && (last_change.get(path).is_some_and(|l| l > at)
                            || last_read.get(sig).is_some_and(|l| l > at))
                });
            if stale && m.content.len() > STUB.len() {
                STUB.to_string()
            } else if bulk && i < old && m.content.len() > MAX {
                format!(
                    "{}\n[older output elided: {} bytes; re-run the tool if needed]",
                    truncate(&m.content, MAX / 2),
                    m.content.len()
                )
            } else {
                continue;
            }
        } else if i < old
            && m.role == Role::Assistant
            && (m
                .reasoning_content
                .as_deref()
                .is_some_and(|r| !r.is_empty())
                || m.tool_calls.iter().any(bulky_payload))
        {
            let mut slim = m.clone();
            slim.reasoning_content = None;
            for c in slim.tool_calls.iter_mut().filter(|c| bulky_payload(c)) {
                let args: Value = serde_json::from_str(c.args()).unwrap_or(Value::Null);
                c.function.arguments =
                    json!({ "path": args["path"], "elided_bytes": c.args().len() }).to_string();
            }
            edits.push((i, slim));
            continue;
        } else {
            continue;
        };
        let tool_call_id = m.tool_call_id.clone();
        edits.push((
            i,
            ChatMessage {
                role: Role::Tool,
                content,
                tool_call_id,
                ..Default::default()
            },
        ));
    }
    edits
}

fn render(msgs: &[ChatMessage]) -> String {
    let mut out = String::new();
    for m in msgs {
        let _ = write!(out, "[{:?}] {}", m.role, m.content);
        if !m.tool_calls.is_empty() {
            let calls: Vec<String> = m
                .tool_calls
                .iter()
                .map(|c| format!("{}({})", c.name(), truncate(c.args(), 200)))
                .collect();
            let _ = write!(out, "\n  calls: {}", calls.join(", "));
        }
        out.push_str("\n\n");
    }
    truncate(&out, 120_000).into_owned()
}

#[cfg(test)]
mod tests {
    use super::ContextComposer;
    use crate::llm::{self, ChatMessage};
    use serde_json::{Value, json};

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
        assert!(c.prune(2, true));
        assert!(c.messages()[1].content.len() < 1000);
        assert_eq!(c.messages()[3].content.len(), 5000);
        assert!(!c.prune(2, true));
    }

    #[test]
    fn prune_if_applies_only_when_worth_it() {
        let mut c = ContextComposer::new("sys".into(), vec![], 100);
        c.push(ChatMessage::tool_result("1", "x".repeat(5000)));
        c.push(ChatMessage::user("mid"));
        c.push(ChatMessage::tool_result("2", "y".repeat(5000)));
        let before = c.messages()[1].content.len();
        assert!(!c.prune_if(2, true, |freed, _, _| {
            assert!(freed > 1000);
            false
        }));
        assert_eq!(c.messages()[1].content.len(), before);
        assert!(c.prune_if(2, true, |_, _, _| true));
        assert!(c.messages()[1].content.len() < before);
        assert!(!c.prune_if(2, true, |_, _, _| panic!("nothing left to free")));
    }

    #[test]
    fn prune_without_bulk_keeps_old_results_but_drops_superseded_reads() {
        let mut c = ContextComposer::new("sys".into(), vec![], 100);
        c.push(call("r1", "read", json!({"path": "a.rs"})));
        c.push(ChatMessage::tool_result("r1", "x".repeat(5000)));
        c.push(call("b1", "bash", json!({"command": "ls"})));
        c.push(ChatMessage::tool_result("b1", "y".repeat(5000)));
        c.push(call("e1", "edit", json!({"path": "a.rs"})));
        c.push(ChatMessage::user("mid"));
        c.push(ChatMessage::user("end"));
        assert!(c.prune(2, false));
        assert!(c.messages()[2].content.starts_with("[superseded"));
        assert_eq!(c.messages()[4].content.len(), 5000);
        assert!(c.prune(2, true));
        assert!(c.messages()[4].content.len() < 1000);
    }

    fn call(id: &str, name: &str, args: Value) -> ChatMessage {
        let mut m = ChatMessage::default();
        m.tool_calls
            .push(llm::ToolCall::new(id, name, args.to_string()));
        m
    }

    #[test]
    fn prune_drops_reads_superseded_by_edits_or_rereads() {
        let mut c = ContextComposer::new("sys".into(), vec![], 100);
        let big = "x".repeat(400);
        c.push(call("r1", "read", json!({"path": "a.rs"})));
        c.push(ChatMessage::tool_result("r1", big.clone()));
        c.push(call("r2", "read", json!({"path": "b.rs"})));
        c.push(ChatMessage::tool_result("r2", big.clone()));
        c.push(call(
            "e1",
            "edit",
            json!({"path": "a.rs", "oldText": "a", "newText": "b"}),
        ));
        c.push(ChatMessage::tool_result("e1", "ok"));
        c.push(call("r3", "read", json!({"path": "b.rs"})));
        c.push(ChatMessage::tool_result("r3", big.clone()));
        assert!(c.prune(100, true));
        let m = c.messages();
        assert!(m[2].content.starts_with("[superseded"), "edited later");
        assert!(m[4].content.starts_with("[superseded"), "re-read later");
        assert_eq!(m[8].content, big, "latest read stays");
        assert_eq!(m[6].content, "ok");
        assert!(!c.prune(100, true));
    }

    #[test]
    fn prune_slims_old_write_payloads_and_reasoning() {
        let mut c = ContextComposer::new("sys".into(), vec![], 100);
        let mut m = call(
            "w1",
            "write",
            json!({"path": "a.rs", "content": "y".repeat(2000)}),
        );
        m.reasoning_content = Some("thinking".into());
        c.push(m);
        c.push(ChatMessage::tool_result("w1", "ok"));
        c.push(ChatMessage::user("u"));
        c.push(ChatMessage::user("v"));
        assert!(c.prune(2, true));
        let a = &c.messages()[1];
        assert!(a.reasoning_content.is_none());
        assert!(a.tool_calls[0].args().len() < 100 && a.tool_calls[0].args().contains("a.rs"));
        assert!(!c.prune(2, true));
    }

    #[test]
    fn the_peak_prompt_survives_a_smaller_later_prompt() {
        let mut ctx = ContextComposer::new("sys".into(), vec![], 1000);
        assert!(!ctx.peaked_over(0.4));
        ctx.set_last_prompt_tokens(450);
        ctx.set_last_prompt_tokens(100);
        assert!(ctx.peaked_over(0.4) && !ctx.peaked_over(0.5));
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
