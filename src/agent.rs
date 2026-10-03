use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::config::{self, AgentDef, Config, Provider, dot};
use crate::llm::{self, ChatMessage, LlmClient, Role, ToolCall};
use crate::socket::Control;
use crate::storage::context::ContextComposer;
use crate::storage::events::{self, EventEmitter};
use crate::storage::util::{relative_path, resolve_path};
use crate::tools::{self, Verdict};

const MAX_TRUNCATIONS: u32 = 3;
const LLM_TIMEOUT_SECS: u64 = 600;
const STOPPED_BY_USER: &str = "(stopped by user via control socket)";
const NUDGE: &str = "[note] End by calling finish.";
const CONTINUE: &str = "Continue from where you left off.";
const TRUNCATED_HINT: &str = "[note] Your last response was cut off by the output limit. Continue with smaller steps: split large writes into several edits.";
const INTERRUPTED: &str = "ERROR: interrupted — genji stopped while this tool was running; it may have partially run. Verify the current state before retrying.";

pub struct Agent {
    pub cfg: Config,
    pub workspace: PathBuf,
    pub def: AgentDef,
    pub agents: BTreeMap<String, AgentDef>,
    /// The agent that spawned this run; set only for subagents.
    pub parent_agent: Option<String>,
    pub instance_id: String,
    pub depth: u32,
    /// The id of the tool call being dispatched, safe for use in a file name.
    pub call_tag: String,
    pub verdict: Option<Verdict>,
    /// Set when a handoff will not be followed because the `--follow` cap is reached.
    pub follow_capped: bool,
    /// How the run ended: done | failed | stopped.
    pub status: &'static str,
    pub llm: LlmClient,
    context: Arc<RwLock<ContextComposer>>,
    control: Option<Arc<Control>>,
    events: EventEmitter,
    tokens_used: i64,
    token_limit: i64,
    started: Instant,
}

pub struct AgentParams {
    pub cfg: Config,
    pub workspace: PathBuf,
    pub def: AgentDef,
    pub agents: BTreeMap<String, AgentDef>,
    pub provider: Provider,
    pub instance_id: String,
    pub parent: Option<String>,
    pub parent_agent: Option<String>,
    pub depth: u32,
    pub resume: bool,
    /// Shown in `instance_start`.
    pub task: String,
    pub control: Option<Arc<Control>>,
    /// Shared with the control socket; replaced by this run's context.
    pub context: Arc<RwLock<ContextComposer>>,
}

/// System prompt: the agent's prompt, environment, project instructions, skills, agents, reporting.
fn build_system(
    workspace: &Path,
    def: &AgentDef,
    agents: &BTreeMap<String, AgentDef>,
    parent_agent: Option<&str>,
) -> Result<String> {
    let has = |t: &str| def.tools.iter().any(|n| n == t);
    let mut s = format!(
        "{}\n\n## Environment\ncwd: {}\nos: {}\ngit repo: {}",
        def.prompt.trim_end(),
        workspace.display(),
        std::env::consts::OS,
        workspace.join(".git").exists()
    );
    let doc = ["AGENTS.md", "CLAUDE.md"]
        .iter()
        .find_map(|n| std::fs::read_to_string(workspace.join(n)).ok());
    if let Some(doc) = doc.filter(|d| !d.trim().is_empty()) {
        s.push_str(&format!("\n\n## Project instructions\n{doc}"));
    }
    let skills = config::skill_list(workspace);
    if has("skill_load") && !skills.is_empty() {
        s.push_str("\n\n## Skills\nLoad one with `skill_load` when it applies.\n");
        for (name, description) in &skills {
            s.push_str(&format!("- {name} — {description}\n"));
        }
    }
    for name in &def.skills {
        let text = config::render_skill(workspace, name)
            .with_context(|| format!("agent `{}` forces skill `{name}`", def.name))?;
        s.push_str(&format!("\n\n{text}"));
    }
    if has("finish") {
        s.push_str("\n\n## Agents\n");
        for a in agents.values() {
            s.push_str(&format!("- {} — {}\n", a.name, a.description));
        }
    }
    if let Some(parent) = parent_agent {
        s.push_str(&format!(
            "\n\n## Reporting\nYou were spawned by the `{parent}` agent. When you stop, call `finish` with status `handoff`, \
             `next.agent` = `{parent}`, and your full report in `next.task`: what you did, the evidence, and where the state lives. \
             Use `blocked` if you cannot proceed."
        ));
    }
    Ok(s)
}

fn is_context_overflow(err: &str) -> bool {
    let e = err.to_lowercase();
    [
        "context_length",
        "context length",
        "maximum context",
        "too many tokens",
        "exceeds the context",
    ]
    .iter()
    .any(|k| e.contains(k))
}

fn sanitize(id: &str) -> String {
    let s: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .take(40)
        .collect();
    if s.is_empty() { "call".into() } else { s }
}

impl Agent {
    /// Start a run, or with `resume` continue the recorded one: its system prompt,
    /// tools, model and context come from the session file so the request is the
    /// same prefix the provider already cached.
    pub fn start(p: AgentParams) -> Result<Self> {
        let session = dot(&p.workspace, "sessions").join(format!("{}.jsonl", p.instance_id));
        let window = p.provider.context_window;
        let token_limit = p.provider.token_limit;
        let (model, seq, tokens_used, pending, recorded) = if p.resume {
            let r = events::replay(&session, window)?;
            (r.model, r.seq, r.tokens_used, r.pending, Some(r.ctx))
        } else {
            (
                p.def
                    .model
                    .clone()
                    .unwrap_or_else(|| p.provider.model.clone()),
                0,
                0,
                Vec::new(),
                None,
            )
        };
        let llm = LlmClient::new(
            p.provider,
            model.clone(),
            p.cfg.llm_max_retries,
            LLM_TIMEOUT_SECS,
        );
        let events = EventEmitter::open(&p.instance_id, &session, seq)?;
        events.instance_start(
            &p.workspace.display().to_string(),
            &p.def.name,
            &model,
            p.parent.as_deref(),
            p.depth,
            &p.task,
            p.resume,
        );
        let ctx = match recorded {
            Some(ctx) => ctx,
            None => {
                let system =
                    build_system(&p.workspace, &p.def, &p.agents, p.parent_agent.as_deref())?;
                let tools = tools::specs(&p.def, p.parent_agent.is_some());
                events.system(&system, &tools);
                ContextComposer::new(system, tools, window)
            }
        };
        *p.context.write().unwrap() = ctx;
        let mut agent = Agent {
            cfg: p.cfg,
            workspace: p.workspace,
            def: p.def,
            agents: p.agents,
            parent_agent: p.parent_agent,
            instance_id: p.instance_id,
            depth: p.depth,
            call_tag: String::new(),
            verdict: None,
            follow_capped: false,
            status: "done",
            llm,
            context: p.context,
            control: p.control,
            events,
            tokens_used,
            token_limit,
            started: Instant::now(),
        };
        // `spawn` and `finish` are safe to run again; any other call may have partly run.
        for tc in pending {
            if matches!(tc.name(), "spawn" | "finish") {
                agent.run_call(&tc);
            } else {
                eprintln!("[resume] {} was running when genji stopped", tc.name());
                agent.log_result(&tc, INTERRUPTED.to_string(), true, Duration::ZERO);
            }
        }
        Ok(agent)
    }

    pub fn resolve_path(&self, path: &str) -> PathBuf {
        resolve_path(&self.workspace, path)
    }

    pub fn display_path(&self, path: &Path) -> String {
        relative_path(&self.workspace, path)
    }

    fn log(&mut self, msg: ChatMessage) {
        match msg.role {
            Role::User => self.events.user(&msg.content),
            Role::Assistant => {
                self.events.assistant(&msg);
                msg.tool_calls.iter().for_each(|c| self.events.tool_call(c));
            }
            Role::System | Role::Tool => {}
        }
        self.context.write().unwrap().push(msg);
    }

    fn log_result(&mut self, tc: &ToolCall, result: String, is_error: bool, took: Duration) {
        let ms = i64::try_from(took.as_millis()).unwrap_or(i64::MAX);
        self.events
            .tool_result(&tc.id, tc.name(), is_error, ms, &result);
        if is_error {
            eprintln!(
                "[tool] {}({}) -> ERROR ({ms}ms)",
                tc.name(),
                llm::truncate(tc.args(), 120)
            );
        }
        self.context
            .write()
            .unwrap()
            .push(ChatMessage::tool_result(&tc.id, result));
    }

    fn run_call(&mut self, tc: &ToolCall) {
        self.call_tag = sanitize(&tc.id);
        let start = Instant::now();
        let (result, is_error) = match serde_json::from_str::<Value>(tc.args()) {
            Ok(args) => tools::dispatch(self, tc.name(), &args),
            Err(e) => (format!("ERROR: invalid JSON tool arguments: {e}"), true),
        };
        self.log_result(tc, result, is_error, start.elapsed());
    }

    /// Run until the agent finishes or stops, then record `instance_end`.
    /// `task` is appended as a user message; a finished session resumed without
    /// one is told to continue.
    pub fn run(&mut self, task: Option<&str>) -> Result<String> {
        let last = self
            .context
            .read()
            .unwrap()
            .messages()
            .last()
            .map(|m| m.role);
        match (task, last) {
            (Some(t), _) => self.log(ChatMessage::user(t)),
            (None, Some(Role::Assistant)) => self.log(ChatMessage::user(CONTINUE)),
            _ => {}
        }
        let report = self
            .run_loop()
            .unwrap_or_else(|e| self.fail(format!("{e:#}")));
        let result = self.verdict.as_ref().map(|v| json!(v));
        if let (true, Some(Verdict { next: Some(n), .. })) = (self.follow_capped, &self.verdict) {
            let msg = format!(
                "handoff limit reached; not following the handoff to `{}`",
                n.agent
            );
            eprintln!("[follow] {msg}");
            self.events.error(&msg);
        }
        self.events
            .instance_end(self.status, self.tokens_used, &report, result.as_ref());
        Ok(report)
    }

    fn fail(&mut self, msg: String) -> String {
        eprintln!("[llm] {msg}");
        self.events.error(&msg);
        self.status = "failed";
        msg
    }

    fn stop(&mut self, msg: String) -> String {
        eprintln!("[stop] {msg}");
        self.events.error(&msg);
        self.status = "stopped";
        format!("({msg})")
    }

    fn budget_exceeded(&self) -> Option<String> {
        if self.tokens_used >= self.token_limit {
            return Some(format!(
                "token limit reached ({} >= {})",
                self.tokens_used, self.token_limit
            ));
        }
        if self.started.elapsed().as_secs() >= self.cfg.time_limit_secs.max(1) {
            return Some(format!(
                "time limit reached ({}s)",
                self.cfg.time_limit_secs
            ));
        }
        None
    }

    fn run_loop(&mut self) -> Result<String> {
        let mut iterations = 0usize;
        let mut truncations = 0u32;
        let mut overflow_retried = false;
        let mut nudged = false;
        // One-request note after the context; never stored.
        let mut hint: Option<&str> = None;
        loop {
            if let Some(reason) = self.budget_exceeded() {
                return Ok(self.stop(reason));
            }
            let (stop, _) = self.poll_control();
            if stop {
                self.events.status(STOPPED_BY_USER);
                self.status = "stopped";
                return Ok(STOPPED_BY_USER.to_string());
            }
            self.set_status();
            self.maybe_compact(self.cfg.compact_threshold, true)?;
            let result = {
                let ctx = self.context.read().unwrap();
                self.llm.chat(ctx.messages(), ctx.tools(), hint.take())
            };
            let resp = match result {
                Ok(r) => r,
                Err(e) if !overflow_retried && is_context_overflow(&format!("{e:#}")) => {
                    overflow_retried = true;
                    eprintln!("[llm] context overflow; compacting and retrying");
                    self.maybe_compact(0.0, false)?;
                    continue;
                }
                Err(e) => return Ok(self.fail(format!("LLM request failed: {e:#}"))),
            };
            self.tokens_used += resp.prompt_tokens + resp.completion_tokens;
            self.context
                .write()
                .unwrap()
                .set_last_prompt_tokens(resp.prompt_tokens);
            self.events.tokens(
                self.tokens_used,
                resp.prompt_tokens,
                resp.completion_tokens,
                resp.cached_tokens,
            );
            if resp.truncated {
                if truncations >= MAX_TRUNCATIONS {
                    return Ok(self.fail(format!(
                        "LLM request failed: response truncated {MAX_TRUNCATIONS} times"
                    )));
                }
                truncations += 1;
                self.events.error(&format!("LLM response truncated (finish_reason=length); retry {truncations}/{MAX_TRUNCATIONS}"));
                hint = Some(TRUNCATED_HINT);
                continue;
            }
            truncations = 0;
            let assistant = resp.message;
            if assistant.tool_calls.is_empty() {
                let text = if assistant.content.trim().is_empty() {
                    "(no output)".to_string()
                } else {
                    assistant.content.clone()
                };
                self.log(assistant);
                let (stop, injected) = self.poll_control();
                if stop {
                    self.status = "stopped";
                    return Ok(STOPPED_BY_USER.to_string());
                }
                if injected {
                    continue;
                }
                if !nudged && self.def.tools.iter().any(|t| t == "finish") {
                    nudged = true;
                    hint = Some(NUDGE);
                    continue;
                }
                return Ok(text);
            }
            nudged = false;
            let mut calls = assistant.tool_calls.clone();
            self.log(assistant);
            // `finish` runs after the other calls of its turn.
            calls.sort_by_key(|c| c.name() == "finish");
            for tc in &calls {
                self.run_call(tc);
            }
            if let Some(v) = &self.verdict {
                return Ok(v.summary.clone());
            }
            iterations += 1;
            if iterations >= self.cfg.max_tool_iterations {
                return Ok(self.stop(format!(
                    "reached max tool iterations {}",
                    self.cfg.max_tool_iterations
                )));
            }
        }
    }

    fn set_status(&self) {
        let status = format!(
            "working agent={} tokens={} elapsed={}s",
            self.def.name,
            self.tokens_used,
            self.started.elapsed().as_secs()
        );
        if let Some(c) = &self.control {
            c.set_status(status.clone());
            self.events.status(&status);
        }
    }

    /// Queued user instructions enter the context. Returns (stop requested, injected).
    fn poll_control(&mut self) -> (bool, bool) {
        let Some(ctrl) = self.control.clone() else {
            return (false, false);
        };
        let queued = ctrl.drain();
        for ins in &queued {
            eprintln!(
                "[control] injecting instruction: {}",
                llm::truncate(ins, 160)
            );
            self.log(ChatMessage::user(format!("[instruction from user]\n{ins}")));
        }
        (ctrl.stop_requested(), !queued.is_empty())
    }

    /// Prune old tool results, then summarize older turns, once the context nears the window.
    /// Both are logged so a resume rebuilds the same context.
    fn maybe_compact(&mut self, threshold: f64, optional: bool) -> Result<()> {
        let keep = self.cfg.compact_keep_recent;
        if !self.context.read().unwrap().over(threshold) {
            return Ok(());
        }
        if self.context.write().unwrap().prune(keep) {
            self.events.prune(keep);
            if !self.context.read().unwrap().over(threshold) {
                return Ok(());
            }
        }
        let Some((rendered, kept)) = self.context.read().unwrap().compaction_source(keep) else {
            return Ok(());
        };
        let req = [
            ChatMessage::system(
                "You compress agent running history. Preserve decisions, key clues, open problems. Be dense and factual.",
            ),
            ChatMessage::user(format!(
                "Summarize this conversation segment:\n\n{}",
                llm::truncate(&rendered, 120_000)
            )),
        ];
        let resp = match self.llm.chat(&req, &[], None) {
            Ok(r) => r,
            // Compaction is an optimisation when under the threshold; keep going without it.
            Err(e) if optional => {
                self.events.error(&format!("compaction failed: {e:#}"));
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        self.tokens_used += resp.prompt_tokens + resp.completion_tokens;
        let summary = resp.message.content.trim().to_string();
        let removed = self.context.read().unwrap().messages().len() - 1 - kept;
        self.events
            .compaction(&summary, kept, removed, self.tokens_used);
        self.context
            .write()
            .unwrap()
            .apply_compaction(&summary, kept);
        eprintln!("[compact] summarized {removed} messages");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::util::temp_dir;

    #[test]
    fn detects_context_overflow_errors() {
        assert!(is_context_overflow(
            "HTTP 400: {\"code\":\"context_length_exceeded\"}"
        ));
        assert!(is_context_overflow("exceeds the context window of 8192"));
        assert!(!is_context_overflow("HTTP 401: invalid api key"));
    }

    #[test]
    fn system_prompt_sections_follow_the_agent() {
        let ws = temp_dir("system");
        let agents = config::load_agents(&ws);
        let plan = build_system(&ws, &agents["plan"], &agents, None).unwrap();
        assert!(
            plan.contains("## Environment")
                && plan.contains("## Agents")
                && plan.contains("- build — ")
        );
        assert!(plan.contains("- formal — ") && !plan.contains("# Skill: formal"));
        assert!(!plan.contains("## Reporting"));
        let sub = build_system(&ws, &agents["explore"], &agents, Some("plan")).unwrap();
        assert!(sub.contains("## Reporting") && sub.contains("`next.agent` = `plan`"));
        let mut forced = agents["build"].clone();
        forced.skills = vec!["formal".into()];
        assert!(
            build_system(&ws, &forced, &agents, None)
                .unwrap()
                .contains("# Skill: formal")
        );
        forced.skills = vec!["nope".into()];
        assert!(build_system(&ws, &forced, &agents, None).is_err());
    }

    #[test]
    fn call_tags_are_file_safe() {
        assert_eq!(sanitize("call_abc/../1"), "call_abc1");
        assert_eq!(sanitize("///"), "call");
    }
}
