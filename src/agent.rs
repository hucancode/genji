use anyhow::Result;
use rusqlite::params;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::{expand_tilde, Config};
use crate::control::{Control, ControlPoll};
use crate::db::Db;
use crate::llm::{self, ChatMessage, LlmClient};
use crate::modes::{shared_preamble, Mode};
use crate::prompts;
use crate::tools::{self, ToolSpec};

#[allow(dead_code)]
pub struct Agent {
    pub cfg: Config,
    pub workspace: PathBuf,
    pub db: Db,
    pub session_id: String,
    pub parent_session: Option<String>,
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
    pub verbose: bool,
    pub control: Option<Arc<Control>>,
}

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Config,
        workspace: PathBuf,
        db: Db,
        session_id: String,
        parent_session: Option<String>,
        mode: Mode,
        model: String,
        depth: u32,
        task: &str,
        interactive: bool,
        control: Option<Arc<Control>>,
    ) -> Result<Self> {
        let limits = cfg.limits_for_model(&model);
        let llm = LlmClient::new(&cfg, &model)?;
        let tools = tools::specs_for(mode);
        let system = build_system(&db, mode)?;
        db.session_start(
            &session_id,
            mode.as_str(),
            parent_session.as_deref(),
            task,
            &model,
            depth,
        )?;
        let deadline = Instant::now() + Duration::from_secs(cfg.time_limit_secs.max(1));
        let verbose = cfg.verbose;
        Ok(Agent {
            cfg,
            workspace,
            db,
            session_id,
            parent_session,
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
            verbose,
            control,
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
            "UPDATE sessions SET mode=? WHERE id=?",
            params![mode.as_str(), self.session_id],
        )?;
        self.refresh_system_prompt()?;
        Ok(())
    }

    /// Append a message to the in-memory conversation and the persistent log.
    pub fn log(&mut self, msg: ChatMessage) -> Result<()> {
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
            &self.session_id,
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
        loop {
            if let Some(reason) = self.budget_exceeded() {
                eprintln!("[budget] {reason}");
                return Ok(format!("(stopped: {reason})"));
            }

            // Mid-run steering: safe here because all tool results from the
            // previous assistant turn have already been appended.
            let poll = self.poll_control();
            if poll.stop {
                let m = "(stopped by user via control socket)".to_string();
                eprintln!("[control] {m}");
                return Ok(m);
            }
            if let Some(c) = &self.control {
                c.set_status(format!(
                    "mode={} model={} tokens={} messages={} session={} elapsed={}s",
                    self.mode.as_str(),
                    self.model,
                    self.tokens_used,
                    self.messages.len(),
                    self.session_id,
                    self.started.elapsed().as_secs()
                ));
            }
            self.maybe_compact()?;

            let tools_json: Vec<Value> = self.tools.iter().map(|t| t.to_json()).collect();
            let resp = match self.llm.chat(&self.messages, &tools_json) {
                Ok(r) => r,
                Err(e) => {
                    let msg = format!("LLM error: {e:#}");
                    eprintln!("[llm] {msg}");
                    let _ = self.log(ChatMessage::assistant(&msg));
                    return Ok(msg);
                }
            };
            self.tokens_used += resp.prompt_tokens + resp.completion_tokens;
            self.last_prompt_tokens = resp.prompt_tokens;
            let _ = self
                .db
                .session_set_tokens(&self.session_id, self.tokens_used);

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
                    &self.session_id,
                    msg_seq,
                    &tc.name,
                    &tc.arguments,
                    &result,
                    is_error,
                    dur,
                )?;
                if self.verbose || is_error {
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
                return Ok(msg);
            }
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
            ChatMessage::user(format!("Summarize this conversation segment:\n\n{rendered}")),
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
            .compaction_add(&self.session_id, removed, before, after, &summary)?;
        eprintln!("[compact] removed {removed} messages ({before} -> {after} est tokens)");
        Ok(())
    }

    pub fn finish(&self, status: &str, report: &str) -> Result<()> {
        self.db
            .session_end(&self.session_id, status, self.tokens_used, report)
    }
}

pub fn build_system(db: &Db, mode: Mode) -> Result<String> {
    if !mode.allows_extended() {
        return Ok(format!("{}\n{}", shared_preamble(), mode.core_prompt()));
    }
    let extended = prompts::load_extended(db, mode)?;
    Ok(format!(
        "{}\n{}\n\n## Extended guidance\n{}",
        shared_preamble(),
        mode.core_prompt(),
        extended
    ))
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
