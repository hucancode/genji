use anyhow::Result;
use rusqlite::params;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{expand_tilde, Config};
use crate::control::{Control, ControlPoll};
use crate::db::Db;
use crate::events::EventEmitter;
use crate::llm::{self, ChatMessage, LlmClient};
use crate::modes::{shared_preamble, Mode};
use crate::prompts;
use crate::registry;
use crate::tools::{self, ToolSpec};

const MAX_LLM_RETRIES: u32 = 3;

pub struct Agent {
    pub cfg: Config,
    pub workspace: PathBuf,
    pub db: Db,
    pub instance_id: String,
    pub mode: Mode,
    pub model: String,
    pub llm: LlmClient,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub tokens_used: i64,
    pub last_prompt_tokens: i64,
    pub token_limit: i64,
    pub context_window: i64,
    pub started: Instant,
    pub deadline: Instant,
    pub depth: u32,
    pub seq: i64,
    pub interactive: bool,
    pub control: Option<Arc<Control>>,
    pub events: Arc<EventEmitter>,
    /// Set when a fatal LLM failure ends the run (see [`Agent::status`]).
    pub failed: bool,
}

/// Everything needed to start an agent run. Kept as a struct so construction
/// stays a single, readable call and the model is resolved in one place.
pub struct AgentParams {
    pub cfg: Config,
    pub workspace: PathBuf,
    pub db: Db,
    pub instance_id: String,
    pub parent_instance: Option<String>,
    pub mode: Mode,
    pub depth: u32,
    pub task: String,
    pub interactive: bool,
    pub control: Option<Arc<Control>>,
}

impl Agent {
    pub fn new(params: AgentParams) -> Result<Self> {
        let AgentParams {
            cfg,
            workspace,
            db,
            instance_id,
            parent_instance,
            mode,
            depth,
            task,
            interactive,
            control,
        } = params;
        // The model is derived here, not passed in, so `Agent` and `set_mode`
        // share one source of truth for model selection.
        let model = cfg.model_for_mode(mode);
        let limits = cfg.limits_for_model(&model);
        let llm = LlmClient::new(&cfg, &model)?;
        let tools = tools::specs_for(mode);
        let system = build_system(&db, mode)?;
        db.instance_start(
            &instance_id,
            mode.as_str(),
            parent_instance.as_deref(),
            &task,
            &model,
            depth,
        )?;
        // stdout is the machine event stream; the same events are appended to a
        // per-instance trace file for `genji inspect`. Owning the emitter here
        // keeps the trace path written in exactly one place.
        let events = Arc::new(EventEmitter::new(
            instance_id.clone(),
            Some(registry::events_path(&instance_id)),
        ));
        events.instance_start(
            &workspace.display().to_string(),
            mode.as_str(),
            &model,
            parent_instance.as_deref(),
            depth,
            &task,
        );
        let deadline = Instant::now() + Duration::from_secs(cfg.time_limit_secs.max(1));
        Ok(Agent {
            cfg,
            workspace,
            db,
            instance_id,
            mode,
            model,
            llm,
            messages: vec![ChatMessage::system(system)],
            tools,
            tokens_used: 0,
            last_prompt_tokens: 0,
            token_limit: limits.token_limit,
            context_window: limits.context_window,
            started: Instant::now(),
            deadline,
            depth,
            seq: 0,
            interactive,
            control,
            events,
            failed: false,
        })
    }

    pub fn resolve_path(&self, p: &str) -> PathBuf {
        let expanded = expand_tilde(p);
        if expanded.is_absolute() {
            expanded
        } else {
            self.workspace.join(expanded)
        }
    }

    pub fn display_path(&self, p: &Path) -> String {
        p.strip_prefix(&self.workspace)
            .unwrap_or(p)
            .to_string_lossy()
            .to_string()
    }

    pub fn refresh_system_prompt(&mut self) -> Result<()> {
        let system = build_system(&self.db, self.mode)?;
        if let Some(first) = self.messages.first_mut() {
            first.content = system;
        } else {
            self.messages.push(ChatMessage::system(system));
        }
        Ok(())
    }

    pub fn set_mode(&mut self, mode: Mode) -> Result<()> {
        self.mode = mode;
        self.model = self.cfg.model_for_mode(mode);
        let limits = self.cfg.limits_for_model(&self.model);
        self.token_limit = limits.token_limit;
        self.context_window = limits.context_window;
        self.llm = LlmClient::new(&self.cfg, &self.model)?;
        self.tools = tools::specs_for(mode);
        self.db.conn.execute(
            "UPDATE instances SET mode=? WHERE id=?",
            params![mode.as_str(), self.instance_id],
        )?;
        self.refresh_system_prompt()?;
        self.events.mode(mode.as_str(), &self.model);
        Ok(())
    }

    /// Append a message to the in-memory conversation and the persistent log.
    pub fn log(&mut self, msg: ChatMessage) -> Result<()> {
        match msg.role.as_str() {
            "user" => self.events.user(&msg.content),
            "assistant" => {
                self.events
                    .assistant(&msg.content, msg.reasoning_content.as_deref());
                for c in &msg.tool_calls {
                    self.events.tool_call(&c.id, &c.name, &c.arguments);
                }
            }
            _ => {}
        }
        let tool_calls_json = if msg.tool_calls.is_empty() {
            None
        } else {
            let arr: Vec<Value> = msg
                .tool_calls
                .iter()
                .map(|c| json!({"id": c.id, "name": c.name, "arguments": c.arguments}))
                .collect();
            Some(serde_json::to_string(&arr)?)
        };
        self.db.message_add(
            &self.instance_id,
            self.seq,
            &msg.role,
            &msg.content,
            tool_calls_json.as_deref(),
            msg.tool_call_id.as_deref(),
            msg.reasoning_content.as_deref(),
        )?;
        self.seq += 1;
        self.messages.push(msg);
        Ok(())
    }

    pub fn add_user(&mut self, text: &str) -> Result<()> {
        self.log(ChatMessage::user(text))
    }

    fn budget_exceeded(&self) -> Option<String> {
        if self.tokens_used >= self.token_limit {
            return Some(format!(
                "token limit reached ({} >= {})",
                self.tokens_used, self.token_limit
            ));
        }
        if Instant::now() >= self.deadline {
            return Some(format!(
                "time limit reached ({}s)",
                self.cfg.time_limit_secs
            ));
        }
        None
    }

    /// Run turns until the model stops calling tools, or a budget/iteration
    /// limit is hit. Returns the final assistant text.
    pub fn run_loop(&mut self) -> Result<String> {
        let mut iterations = 0usize;
        let mut llm_retries = 0u32;
        loop {
            if let Some(reason) = self.budget_exceeded() {
                eprintln!("[budget] {reason}");
                self.events.error(&format!("stopped: {reason}"));
                return Ok(format!("(stopped: {reason})"));
            }

            // Mid-run steering: safe here because all tool results from the
            // previous assistant turn have already been appended.
            let poll = self.poll_control();
            if poll.stop {
                let m = "(stopped by user via control socket)".to_string();
                eprintln!("[control] {m}");
                self.events.status(&m);
                return Ok(m);
            }
            if let Some(c) = &self.control {
                let status = format!(
                    "working mode={} tokens={} elapsed={}s",
                    self.mode.as_str(),
                    self.tokens_used,
                    self.started.elapsed().as_secs()
                );
                c.set_status(status.clone());
                self.events.status(&status);
            }
            self.maybe_compact()?;

            let tools_json: Vec<Value> = self.tools.iter().map(|t| t.to_json()).collect();
            let resp = match self.llm.chat(&self.messages, &tools_json) {
                Ok(r) => r,
                Err(e) => return Ok(self.fail(format!("LLM request failed: {e:#}"))),
            };
            self.tokens_used += resp.prompt_tokens + resp.completion_tokens;
            self.last_prompt_tokens = resp.prompt_tokens;
            self.events
                .tokens(self.tokens_used, resp.prompt_tokens, resp.completion_tokens);
            let _ = self
                .db
                .instance_set_tokens(&self.instance_id, self.tokens_used);

            if resp.is_truncated() {
                if llm_retries >= MAX_LLM_RETRIES {
                    return Ok(self.fail(format!(
                        "LLM request failed: response truncated {MAX_LLM_RETRIES} times"
                    )));
                }
                llm_retries += 1;
                let msg = format!(
                    "LLM response truncated (finish_reason=length); retry {llm_retries}/{MAX_LLM_RETRIES}"
                );
                eprintln!("[llm] {msg}");
                self.events.error(&msg);
                std::thread::sleep(Duration::from_millis(500 * u64::from(llm_retries)));
                continue;
            }
            llm_retries = 0;

            let assistant = resp.message.clone();
            if assistant.tool_calls.is_empty() {
                let text = if assistant.content.trim().is_empty() {
                    "(no output)".to_string()
                } else {
                    assistant.content.clone()
                };
                // Preserve any reasoning in the log.
                self.log(assistant)?;
                // An instruction may have arrived while producing this final
                // message; if so, keep going rather than ending the run.
                let poll = self.poll_control();
                if poll.stop {
                    return Ok("(stopped by user via control socket)".to_string());
                }
                if poll.injected > 0 {
                    continue;
                }
                return Ok(text);
            }

            let tool_calls = assistant.tool_calls.clone();
            self.log(assistant)?;
            let msg_seq = self.seq - 1;

            for tc in tool_calls {
                let args: Value = serde_json::from_str(&tc.arguments).unwrap_or_else(|_| json!({}));
                let start = Instant::now();
                let (result, is_error) = tools::dispatch(self, &tc.name, &args);
                let dur = start.elapsed().as_millis() as i64;
                self.db.tool_call_add(
                    &self.instance_id,
                    msg_seq,
                    &tc.name,
                    &tc.arguments,
                    &result,
                    is_error,
                    dur,
                )?;
                self.events
                    .tool_result(&tc.id, &tc.name, is_error, dur, &result);
                if self.cfg.verbose || is_error {
                    eprintln!(
                        "[tool] {}({}) -> {} ({}ms)",
                        tc.name,
                        llm::truncate(tc.arguments.clone(), 120),
                        if is_error { "ERROR" } else { "ok" },
                        dur
                    );
                }
                self.log(ChatMessage::tool_result(&tc.id, result))?;
            }

            iterations += 1;
            if iterations >= self.cfg.max_tool_iterations {
                let msg = format!(
                    "(stopped: reached max tool iterations {})",
                    self.cfg.max_tool_iterations
                );
                eprintln!("[loop] {msg}");
                self.events.error(&msg);
                return Ok(msg);
            }
        }
    }

    fn fail(&mut self, msg: String) -> String {
        eprintln!("[llm] {msg}");
        self.events.error(&msg);
        let _ = self.log(ChatMessage::assistant(&msg));
        self.failed = true;
        msg
    }

    /// Run outcome to record with [`Agent::finish`].
    pub fn status(&self) -> &'static str {
        if self.failed {
            "failed"
        } else {
            "done"
        }
    }

    fn poll_control(&mut self) -> ControlPoll {
        let Some(ctrl) = self.control.clone() else {
            return ControlPoll::default();
        };
        let mut out = ControlPoll::default();
        for ins in ctrl.drain() {
            eprintln!(
                "[control] injecting instruction: {}",
                llm::truncate(ins.clone(), 160)
            );
            let _ = self.log(ChatMessage::user(format!("[instruction from user]\n{ins}")));
            out.injected += 1;
        }
        if ctrl.stop_requested() {
            out.stop = true;
        }
        out
    }

    fn maybe_compact(&mut self) -> Result<()> {
        let threshold = (self.context_window as f64 * self.cfg.compact_threshold) as i64;
        let est = if self.last_prompt_tokens > 0 {
            self.last_prompt_tokens
        } else {
            llm::estimate_messages(&self.messages)
        };
        if est >= threshold {
            self.compact()?;
        }
        Ok(())
    }

    fn compact(&mut self) -> Result<()> {
        let keep = self.cfg.compact_keep_recent.max(2);
        if self.messages.len() <= keep + 2 {
            return Ok(());
        }
        let mut split = self.messages.len() - keep;
        // Never start the kept window with an orphaned tool result.
        while split < self.messages.len() && self.messages[split].role == "tool" {
            split += 1;
        }
        if split <= 1 {
            return Ok(());
        }
        let before = llm::estimate_messages(&self.messages);
        let middle: Vec<ChatMessage> = self.messages[1..split].to_vec();
        let rendered = llm::truncate(render_messages(&middle), 120_000);
        let summary_req = vec![
            ChatMessage::system(
                "You compress agent conversation history. Preserve decisions, file paths, \
                 tool outcomes, open problems, requirements and ticket ids. Be dense and factual.",
            ),
            ChatMessage::user(format!(
                "Summarize this conversation segment:\n\n{rendered}"
            )),
        ];
        let resp = self.llm.chat(&summary_req, &[])?;
        let summary = resp.message.content.trim().to_string();
        self.tokens_used += resp.prompt_tokens + resp.completion_tokens;

        let system = self.messages[0].clone();
        let recent: Vec<ChatMessage> = self.messages[split..].to_vec();
        let removed = (split - 1) as i64;
        let mut new_msgs = vec![system];
        new_msgs.push(ChatMessage::user(format!(
            "[compacted summary of earlier conversation]\n{summary}"
        )));
        new_msgs.extend(recent);
        self.messages = new_msgs;
        let after = llm::estimate_messages(&self.messages);
        self.last_prompt_tokens = 0;
        self.db
            .compaction_add(&self.instance_id, removed, before, after, &summary)?;
        eprintln!("[compact] removed {removed} messages ({before} -> {after} est tokens)");
        self.events.compaction(removed, before, after, &summary);
        Ok(())
    }

    pub fn finish(&self, status: &str, report: &str) -> Result<()> {
        self.events.instance_end(status, self.tokens_used, report);
        self.db
            .instance_end(&self.instance_id, status, self.tokens_used, report)
    }
}

pub fn build_system(db: &Db, mode: Mode) -> Result<String> {
    let base = format!("{}\n{}", shared_preamble(), mode.core_prompt());
    // `load_extended` is the single gate for which modes have an extended
    // prompt; it returns empty for RETRO and for an unedited (empty) prompt.
    let extended = prompts::load_extended(db, mode)?;
    if extended.trim().is_empty() {
        return Ok(base);
    }
    Ok(format!("{base}\n\n## Extended guidance\n{extended}"))
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
