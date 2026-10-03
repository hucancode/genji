use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::config::{Config, ModelRuntime};
use crate::llm::{self, ChatMessage, LlmClient, Role, ToolCall};
use crate::socket::{Control, ControlPoll};
use crate::storage::context::ContextComposer;
use crate::storage::db::{CallRow, Db, ResumeState};
use crate::storage::events::EventEmitter;
use crate::storage::modes::{Mode, SHARED_PREAMBLE};
use crate::tools::{self, Recovery};

const MAX_LLM_RETRIES: u32 = 3;
/// Process exit code: 0 done, 1 LLM failure, 2 stopped by a limit or the user.
pub static EXIT_CODE: AtomicI32 = AtomicI32::new(0);
const MAX_REPEATED_CALLS: usize = 3;
const TOOL_FAILED_HINT: &str =
    "A tool call failed; read its error and fix the cause instead of repeating it.";
const STOPPED_BY_USER: &str = "(stopped by user via control socket)";
const INTERRUPTED: &str = "ERROR: interrupted — genji stopped while this tool was running; it may \
     have partially run. Verify the current state before retrying.";

pub struct Agent {
    pub cfg: Config,
    pub workspace: PathBuf,
    pub db: Db,
    pub instance_id: String,
    pub mode: Mode,
    pub llm: LlmClient,
    pub context: Arc<RwLock<ContextComposer>>,
    pub tokens_used: i64,
    pub token_limit: i64,
    pub started: Instant,
    pub depth: u32,
    pub seq: i64,
    pub formal: bool,
    pub active_plan: Option<String>,
    pub control: Option<Arc<Control>>,
    pub events: EventEmitter,
    pub failed: bool,
    /// Journal row of the tool call being dispatched.
    pub running_call: Option<i64>,
    /// Skills loaded into the system prompt, in load order.
    skills: Vec<String>,
    last_call: Option<(String, String)>,
    repeats: usize,
}

pub struct AgentParams {
    pub cfg: Config,
    pub workspace: PathBuf,
    pub db: Db,
    pub instance_id: String,
    pub parent_instance: Option<String>,
    pub mode: Mode,
    pub depth: u32,
    pub task: String,
    /// Continue this recorded instance instead of starting a new one.
    pub resume: Option<ResumeState>,
    pub formal: bool,
    pub control: Option<Arc<Control>>,
    pub context: Arc<RwLock<ContextComposer>>,
    /// Resolved runtime for `mode`.
    pub runtime: ModelRuntime,
}

pub fn build_context(
    cfg: &Config,
    workspace: &Path,
    mode: Mode,
    formal: bool,
    context_window: i64,
) -> Arc<RwLock<ContextComposer>> {
    let system = build_system(cfg, workspace, mode, formal);
    Arc::new(RwLock::new(ContextComposer::new(
        system,
        mode_tools(cfg, workspace, mode, formal),
        context_window,
    )))
}

fn mode_tools(cfg: &Config, workspace: &Path, mode: Mode, formal: bool) -> Vec<Value> {
    tools::specs_for(mode, formal, tools::skills::any(cfg, workspace))
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
            resume,
            formal,
            control,
            context,
            runtime,
        } = params;
        let model = runtime.model.clone();
        let limits = runtime.limits;
        let llm = LlmClient::from_runtime(&cfg, runtime);
        let events = EventEmitter::new(instance_id.clone());
        match &resume {
            Some(_) => db.instance_resume(&instance_id)?,
            None => db.instance_start(
                &instance_id,
                mode.as_str(),
                parent_instance.as_deref(),
                &task,
                &model,
                depth,
            )?,
        }
        events.instance_start(
            &workspace.display().to_string(),
            mode.as_str(),
            &model,
            parent_instance.as_deref(),
            depth,
            &task,
            resume.is_some(),
        );
        let (seq, tokens_used) = resume
            .as_ref()
            .map_or((0, 0), |r| (r.next_seq, r.tokens_used));
        let mut agent = Agent {
            cfg,
            workspace,
            db,
            instance_id,
            mode,
            llm,
            context,
            tokens_used,
            token_limit: limits.token_limit,
            started: Instant::now(),
            depth,
            seq,
            formal,
            active_plan: None,
            control,
            events,
            failed: false,
            running_call: None,
            skills: Vec::new(),
            last_call: None,
            repeats: 0,
        };
        if let Some(r) = resume {
            {
                let mut ctx = agent.context.write().unwrap();
                for m in r.messages {
                    ctx.push(m);
                }
            }
            if !r.skills.is_empty() {
                agent.skills = r.skills;
                let system = agent.compose_system();
                agent.context.write().unwrap().set_system(system);
            }
            agent.settle(r.unanswered, r.call_seq)?;
        }
        Ok(agent)
    }

    pub fn resolve_path(&self, path: &str) -> PathBuf {
        crate::storage::util::resolve_path(&self.workspace, path)
    }

    pub fn display_path(&self, path: &Path) -> String {
        crate::storage::util::relative_path(&self.workspace, path)
    }

    /// The system prompt plus the active-plan and loaded-skills sections.
    fn compose_system(&self) -> String {
        let mut system = build_system(&self.cfg, &self.workspace, self.mode, self.formal);
        if let Some(plan) = self.plan_section() {
            system.push_str(&plan);
        }
        let skills: Vec<String> = self
            .skills
            .iter()
            .filter_map(|name| tools::skills::render(&self.cfg, &self.workspace, name).ok())
            .collect();
        if !skills.is_empty() {
            system.push_str("\n\n## Loaded skills\n\n");
            system.push_str(&skills.join("\n\n"));
        }
        system
    }

    /// Plan mode updates the plan (creating it when missing); other modes
    /// follow an existing plan and ignore a missing one.
    fn plan_section(&self) -> Option<String> {
        let slug = self.active_plan.as_ref()?;
        let file = self.cfg.plan_file(&self.workspace, slug);
        let path = self.display_path(&file);
        let exists = std::fs::read_to_string(&file).is_ok_and(|c| !c.trim().is_empty());
        let guidance = match (exists, self.mode == Mode::Plan) {
            (true, true) => format!(
                "Read it before acting, update it with `plan_write` (path `{path}`) when the \
                 approach changes, and treat it as the source of truth."
            ),
            (true, false) => "Read it before acting, treat it as the source of truth, and \
                              report any changes it needs."
                .to_string(),
            (false, true) => format!(
                "No plan file exists yet; create it with `plan_write` (path `{path}`) before \
                 acting, then keep it up to date."
            ),
            (false, false) => return None,
        };
        Some(format!(
            "\n\n## Active plan\n\nThe user selected plan `{slug}` at `{path}`. {guidance}"
        ))
    }

    #[cfg(feature = "formal")]
    pub fn set_mode(&mut self, mode: Mode) -> Result<()> {
        self.mode = mode;
        let runtime = self.cfg.runtime_for_mode(mode)?;
        let limits = runtime.limits;
        self.token_limit = limits.token_limit;
        self.llm = LlmClient::from_runtime(&self.cfg, runtime);
        let tools = mode_tools(&self.cfg, &self.workspace, mode, self.formal);
        let system = self.compose_system();
        self.context
            .write()
            .unwrap()
            .switch_mode(tools, system, limits.context_window);
        self.db
            .instance_set_mode(&self.instance_id, mode.as_str())?;
        self.events.mode(mode.as_str(), &self.llm.model);
        Ok(())
    }

    fn persist(&mut self, msg: &ChatMessage) -> Result<()> {
        match msg.role {
            Role::User => self.events.user(&msg.content),
            Role::Assistant => {
                self.events
                    .assistant(&msg.content, msg.reasoning_content.as_deref());
                for c in &msg.tool_calls {
                    self.events.tool_call(&c.id, &c.name, &c.arguments);
                }
            }
            Role::System | Role::Tool => {}
        }
        self.store(msg)?;
        self.seq += 1;
        Ok(())
    }

    /// Insert `msg` at the current seq; the caller advances `seq` once committed.
    fn store(&self, msg: &ChatMessage) -> Result<()> {
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
            msg.role.as_str(),
            &msg.content,
            tool_calls_json.as_deref(),
            msg.tool_call_id.as_deref(),
            msg.reasoning_content.as_deref(),
        )
    }

    pub fn log(&mut self, msg: ChatMessage) -> Result<()> {
        self.persist(&msg)?;
        self.context.write().unwrap().push(msg);
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
        if self.started.elapsed().as_secs() >= self.cfg.time_limit_secs.max(1) {
            return Some(format!(
                "time limit reached ({}s)",
                self.cfg.time_limit_secs
            ));
        }
        None
    }

    pub fn run_loop(&mut self) -> Result<String> {
        let mut iterations = 0usize;
        let mut rejects = 0u32;
        let mut overflow_retried = false;
        // One-request nudge appended after the context; never stored.
        let mut hint: Option<String> = None;
        loop {
            if let Some(reason) = self.budget_exceeded() {
                eprintln!("[budget] {reason}");
                self.events.error(&format!("stopped: {reason}"));
                EXIT_CODE.store(2, Ordering::Relaxed);
                return Ok(format!("(stopped: {reason})"));
            }
            let poll = self.poll_control();
            if poll.stop {
                eprintln!("[control] {STOPPED_BY_USER}");
                self.events.status(STOPPED_BY_USER);
                EXIT_CODE.store(2, Ordering::Relaxed);
                return Ok(STOPPED_BY_USER.to_string());
            }
            if let Some(c) = &self.control {
                let status = format!(
                    "working mode={} tokens={} elapsed={}s",
                    self.mode.as_str(),
                    self.tokens_used,
                    self.started.elapsed().as_secs()
                );
                let status = match &self.active_plan {
                    Some(p) => format!("{status} plan={p}"),
                    None => status,
                };
                c.set_status(status.clone());
                self.events.status(&status);
            }
            self.maybe_compact(self.cfg.compact_threshold)?;
            let result = {
                let ctx = self.context.read().unwrap();
                self.llm.chat(ctx.messages(), ctx.tools(), hint.as_deref())
            };
            let resp = match result {
                Ok(r) => r,
                Err(e) if !overflow_retried && is_context_overflow(&format!("{e:#}")) => {
                    overflow_retried = true;
                    eprintln!("[llm] context overflow; compacting and retrying");
                    self.maybe_compact(0.0)?;
                    continue;
                }
                Err(e) => return Ok(self.fail(format!("LLM request failed: {e:#}"))),
            };
            self.record_usage(resp.prompt_tokens, resp.completion_tokens);
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
            if resp.is_truncated() {
                if rejects >= MAX_LLM_RETRIES {
                    return Ok(self.fail(format!(
                        "LLM request failed: response truncated {MAX_LLM_RETRIES} times"
                    )));
                }
                rejects += 1;
                let msg = format!(
                    "LLM response truncated (finish_reason=length); retry {rejects}/{MAX_LLM_RETRIES}"
                );
                eprintln!("[llm] {msg}");
                self.events.error(&msg);
                hint = Some(
                    "Your last response was cut off by the output limit. Continue with smaller \
                     steps: split large writes into several edits."
                        .into(),
                );
                continue;
            }
            if rejects < MAX_LLM_RETRIES
                && let Some(problems) = self.malformed_calls(&resp.message.tool_calls)
            {
                rejects += 1;
                let msg = format!(
                    "malformed tool call; retry {rejects}/{MAX_LLM_RETRIES}: {problems}"
                );
                eprintln!("[llm] {msg}");
                self.events.error(&msg);
                hint = Some(format!(
                    "Your last response was discarded because of invalid tool calls: {problems}. \
                     Issue corrected calls."
                ));
                continue;
            }
            rejects = 0;
            hint = None;

            let mut assistant = resp.message;
            let (skill_calls, calls): (Vec<_>, Vec<_>) = std::mem::take(&mut assistant.tool_calls)
                .into_iter()
                .partition(|c| c.name == "skill_load");
            assistant.tool_calls = calls;
            let skill_hint = self.load_skills(&skill_calls)?;
            if !skill_calls.is_empty() && assistant.tool_calls.is_empty() {
                // A turn that only loads skills leaves nothing in the conversation.
                hint = skill_hint;
                iterations += 1;
                continue;
            }
            if assistant.tool_calls.is_empty() {
                let text = if assistant.content.trim().is_empty() {
                    "(no output)".to_string()
                } else {
                    assistant.content.clone()
                };
                self.log(assistant)?;
                let poll = self.poll_control();
                if poll.stop {
                    return Ok(STOPPED_BY_USER.to_string());
                }
                if poll.injected {
                    continue;
                }
                return Ok(text);
            }

            let tool_calls = assistant.tool_calls.clone();
            self.log(assistant)?;
            let msg_seq = self.seq - 1;

            hint = self.execute_tool_calls(&tool_calls, msg_seq)?.or(skill_hint);

            iterations += 1;
            if iterations >= self.cfg.max_tool_iterations {
                let msg = format!(
                    "(stopped: reached max tool iterations {})",
                    self.cfg.max_tool_iterations
                );
                eprintln!("[loop] {msg}");
                self.events.error(&msg);
                EXIT_CODE.store(2, Ordering::Relaxed);
                return Ok(msg);
            }
        }
    }

    /// Problems that make the calls unrunnable, or `None` when they are all fine.
    fn malformed_calls(&self, calls: &[llm::ToolCall]) -> Option<String> {
        let problems: Vec<String> = calls
            .iter()
            .filter_map(|c| {
                tools::validate(self.mode, self.formal, &c.name, &c.arguments).err()
            })
            .collect();
        (!problems.is_empty()).then(|| problems.join("; "))
    }

    /// Runs the calls, storing their results. Returns a hint for the next
    /// request when something went wrong or the model is looping.
    fn execute_tool_calls(&mut self, calls: &[ToolCall], msg_seq: i64) -> Result<Option<String>> {
        let mut hint = None;
        for tc in calls {
            let is_error = self.run_call(tc, msg_seq)?;
            let key = (tc.name.clone(), tc.arguments.clone());
            self.repeats = if self.last_call.as_ref() == Some(&key) {
                self.repeats + 1
            } else {
                1
            };
            self.last_call = Some(key);
            if is_error {
                hint = Some(TOOL_FAILED_HINT.to_string());
            }
            if self.repeats >= MAX_REPEATED_CALLS {
                hint = Some(format!(
                    "The same call has now run {} times in a row; change approach.",
                    self.repeats
                ));
            }
        }
        Ok(hint)
    }

    /// Journal the call as started, run it, and store its result. Returns whether it failed.
    fn run_call(&mut self, tc: &ToolCall, msg_seq: i64) -> Result<bool> {
        let row = self
            .db
            .tool_call_start(&self.instance_id, msg_seq, &tc.id, &tc.name, &tc.arguments)?;
        self.running_call = Some(row);
        let start = Instant::now();
        let (result, is_error) = match serde_json::from_str::<Value>(&tc.arguments) {
            Ok(args) => tools::dispatch(self, &tc.name, &args),
            Err(e) => (format!("ERROR: invalid JSON tool arguments: {e}"), true),
        };
        self.running_call = None;
        self.finish_call(row, tc, result, is_error, start)?;
        Ok(is_error)
    }

    /// Store the result row and its tool message in one transaction, then add it to the context.
    fn finish_call(
        &mut self,
        row: i64,
        tc: &ToolCall,
        result: String,
        is_error: bool,
        start: Instant,
    ) -> Result<()> {
        let duration = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
        let msg = ChatMessage::tool_result(&tc.id, result);
        let tx = self.db.conn.unchecked_transaction()?;
        self.db
            .tool_call_finish(row, "done", &msg.content, is_error, duration)?;
        self.store(&msg)?;
        tx.commit()?;
        self.seq += 1;
        self.events
            .tool_result(&tc.id, &tc.name, is_error, duration, &msg.content);
        if is_error {
            eprintln!(
                "[tool] {}({}) -> ERROR ({}ms)",
                tc.name,
                llm::truncate(&tc.arguments, 120),
                duration
            );
        }
        self.context.write().unwrap().push(msg);
        Ok(())
    }

    /// Answer the calls a stopped run left without results: run the ones that
    /// never started, and recover the ones that were running per their tool.
    fn settle(&mut self, unanswered: Vec<(ToolCall, Option<CallRow>)>, msg_seq: i64) -> Result<()> {
        for (tc, row) in unanswered {
            let Some(row) = row.filter(|r| r.status == "started") else {
                self.run_call(&tc, msg_seq)?;
                continue;
            };
            eprintln!("[resume] {} was running when genji stopped", tc.name);
            match tools::recovery(&tc.name) {
                Recovery::Rerun => {
                    self.db
                        .tool_call_finish(row.id, "interrupted", "", true, 0)?;
                    self.run_call(&tc, msg_seq)?;
                }
                Recovery::Report => {
                    self.finish_call(row.id, &tc, INTERRUPTED.to_string(), true, Instant::now())?;
                }
                Recovery::Reattach => {
                    let start = Instant::now();
                    self.running_call = Some(row.id);
                    let (result, is_error) = match tools::spawn::reattach(self, &row) {
                        Ok(s) => (s, false),
                        Err(e) => (format!("ERROR: {e:#}"), true),
                    };
                    self.running_call = None;
                    self.finish_call(row.id, &tc, result, is_error, start)?;
                }
            }
        }
        Ok(())
    }

    /// Load the requested skills into the system prompt. The calls are
    /// journaled and emitted as events but never enter the conversation.
    /// Returns a hint for the next request when a load failed.
    fn load_skills(&mut self, calls: &[ToolCall]) -> Result<Option<String>> {
        let mut errors = Vec::new();
        let before = self.skills.len();
        for tc in calls {
            let loaded = serde_json::from_str::<Value>(&tc.arguments)
                .map_err(anyhow::Error::from)
                .and_then(|args| tools::skills::requested(&args))
                .and_then(|name| {
                    tools::skills::render(&self.cfg, &self.workspace, &name)?;
                    Ok(name)
                });
            let (result, is_error) = match loaded {
                Ok(name) if self.skills.contains(&name) => {
                    (format!("skill `{name}` is already loaded"), false)
                }
                Ok(name) => {
                    let text = format!("skill `{name}` loaded into the system prompt");
                    self.skills.push(name);
                    (text, false)
                }
                Err(e) => {
                    let text = format!("ERROR: {e:#}");
                    errors.push(text.clone());
                    (text, true)
                }
            };
            self.db.tool_call_record(
                &self.instance_id,
                self.seq,
                &tc.id,
                &tc.name,
                &tc.arguments,
                &result,
                is_error,
            )?;
            self.events.tool_call(&tc.id, &tc.name, &tc.arguments);
            self.events.tool_result(&tc.id, &tc.name, is_error, 0, &result);
        }
        if self.skills.len() > before {
            let system = self.compose_system();
            self.context.write().unwrap().set_system(system);
        }
        Ok((!errors.is_empty()).then(|| format!("skill_load failed: {}", errors.join("; "))))
    }

    fn fail(&mut self, msg: String) -> String {
        eprintln!("[llm] {msg}");
        self.events.error(&msg);
        self.failed = true;
        EXIT_CODE.store(1, Ordering::Relaxed);
        msg
    }

    pub fn status(&self) -> &'static str {
        if self.failed { "failed" } else { "done" }
    }

    fn poll_control(&mut self) -> ControlPoll {
        let Some(ctrl) = self.control.clone() else {
            return ControlPoll::default();
        };
        let mut out = ControlPoll::default();
        let plan = ctrl.active_plan();
        if plan != self.active_plan {
            match &plan {
                Some(p) => eprintln!("[control] following plan: {p}"),
                None => eprintln!("[control] plan cleared"),
            }
            self.active_plan = plan;
            let system = self.compose_system();
            self.context.write().unwrap().set_system(system);
        }
        for ins in ctrl.drain() {
            eprintln!(
                "[control] injecting instruction: {}",
                llm::truncate(&ins, 160)
            );
            let _ = self.log(ChatMessage::user(format!("[instruction from user]\n{ins}")));
            out.injected = true;
        }
        if ctrl.stop_requested() {
            out.stop = true;
        }
        out
    }

    fn record_usage(&mut self, prompt: i64, completion: i64) {
        self.tokens_used += prompt + completion;
        let _ = self
            .db
            .instance_set_tokens(&self.instance_id, self.tokens_used);
    }

    fn maybe_compact(&mut self, threshold: f64) -> Result<()> {
        let compacted = self.context.write().unwrap().maybe_compact(
            threshold,
            self.cfg.compact_keep_recent,
            &self.llm,
        );
        let compacted = match compacted {
            Ok(c) => c,
            Err(e) if threshold > 0.0 => {
                // Compaction is an optimisation here; keep going without it.
                let msg = format!("compaction failed: {e:#}");
                eprintln!("[compact] {msg}");
                self.events.error(&msg);
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let tx = self.db.conn.unchecked_transaction()?;
        self.checkpoint()?;
        if let Some(c) = &compacted {
            self.db
                .compaction_add(&self.instance_id, c.removed, c.before, c.after, &c.summary)?;
        }
        tx.commit()?;
        let Some(c) = compacted else {
            return Ok(());
        };
        self.record_usage(c.prompt_tokens, c.completion_tokens);
        eprintln!(
            "[compact] removed {} messages ({} -> {} est tokens)",
            c.removed, c.before, c.after
        );
        self.events
            .compaction(c.removed, c.before, c.after, &c.summary);
        Ok(())
    }

    /// Persist the context if pruning or compaction rewrote earlier messages.
    fn checkpoint(&self) -> Result<()> {
        let mut ctx = self.context.write().unwrap();
        if ctx.take_rewritten() {
            self.db
                .checkpoint_add(&self.instance_id, self.seq, &ctx.messages()[1..])?;
        }
        Ok(())
    }

    pub fn finish(&self, status: &str, report: &str) -> Result<()> {
        self.events.instance_end(status, self.tokens_used, report);
        self.db
            .instance_end(&self.instance_id, status, self.tokens_used, report)
    }
}

fn is_context_overflow(err: &str) -> bool {
    let e = err.to_lowercase();
    e.contains("context_length")
        || e.contains("context length")
        || e.contains("maximum context")
        || e.contains("too many tokens")
        || e.contains("exceeds the context")
}

/// Core prompt, optional formal guidance, and the user-editable extended prompt
/// from `<prompts_dir>/<mode>.md` (retro has none).
pub fn build_system(cfg: &Config, workspace: &Path, mode: Mode, formal: bool) -> String {
    let mut base = format!("{}\n{}", SHARED_PREAMBLE, mode.core_prompt());
    if formal && !mode.formal_guidance().trim().is_empty() {
        base.push('\n');
        base.push_str(mode.formal_guidance());
    }
    let extended = mode
        .allows_extended()
        .then(|| cfg.prompts_path(workspace).join(format!("{mode}.md")))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .unwrap_or_default();
    base.push_str(&format!(
        "\n\n## Environment\ncwd: {}\nos: {}\ngit repo: {}",
        workspace.display(),
        std::env::consts::OS,
        workspace.join(".git").exists()
    ));
    if let Ok(doc) = ["AGENTS.md", "CLAUDE.md"]
        .iter()
        .map(|n| std::fs::read_to_string(workspace.join(n)))
        .find(Result::is_ok)
        .unwrap_or_else(|| Ok(String::new()))
        && !doc.trim().is_empty()
    {
        base.push_str(&format!("\n\n## Project instructions\n{doc}"));
    }
    if extended.trim().is_empty() {
        return base;
    }
    format!("{base}\n\n## Extended guidance\n{extended}")
}

#[cfg(test)]
mod tests {
    use super::{build_system, is_context_overflow};
    use crate::config::Config;
    use crate::storage::modes::Mode;

    #[test]
    fn extended_prompt_comes_from_the_prompts_dir() {
        let ws = crate::storage::util::temp_dir("agent-prompts");
        let cfg = Config::default();
        let dir = cfg.prompts_path(&ws);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!build_system(&cfg, &ws, Mode::Build, false).contains("Extended guidance"));
        std::fs::write(dir.join("build.md"), "Always run tests.").unwrap();
        std::fs::write(dir.join("retro.md"), "ignored").unwrap();
        assert!(build_system(&cfg, &ws, Mode::Build, false).ends_with("Always run tests."));
        assert!(!build_system(&cfg, &ws, Mode::Retro, false).contains("ignored"));
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn detects_context_overflow_errors() {
        assert!(is_context_overflow(
            "HTTP 400: {\"code\":\"context_length_exceeded\"}"
        ));
        assert!(is_context_overflow("exceeds the context window of 8192"));
        assert!(!is_context_overflow("HTTP 401: invalid api key"));
    }
}
