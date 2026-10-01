//! Context composition and budget accounting.
//!
//! [`ContextComposer`] owns everything that defines the prompt sent to the
//! model: the system prompt, the conversation turns, the tool definitions, and
//! the size of the context window. The agent drives it — pushing messages and
//! tool results, switching mode, compacting — instead of managing the message
//! vector itself. This keeps context policy (what counts toward the window,
//! when to compact) in one place.
//!
//! The prompt has three parts: the system prompt, the tool definitions
//! (`system tools`), and the conversation turns. The breakdown is computed on
//! demand by [`ContextComposer::stats`], which only measures the existing
//! strings: it clones nothing and keeps no counters to drift out of sync.

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
    /// Actual prompt size reported by the provider on the last call, used in
    /// preference to the estimate when deciding whether to compact.
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
    #[cfg(feature = "formal")]
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
        let mut ctx = ContextComposer::new("you are genji".to_string(), vec![tool("read")], 1000);
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
