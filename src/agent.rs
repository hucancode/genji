use anyhow::Result;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use crate::config::{Config, ModelRuntime};
use crate::llm::{self, ChatMessage, LlmClient, Role};
use crate::socket::{Control, ControlPoll};
use crate::storage::context::ContextComposer;
use crate::storage::db::Db;
use crate::storage::events::EventEmitter;
use crate::storage::modes::{Mode, SHARED_PREAMBLE};
use crate::tools;

const MAX_LLM_RETRIES: u32 = 3;
const STOPPED_BY_USER: &str = "(stopped by user via control socket)";

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
            formal,
            control,
            context,
            runtime,
        } = params;
        let model = runtime.model.clone();
        let limits = runtime.limits;
        let llm = LlmClient::from_runtime(&cfg, runtime);
        let events = EventEmitter::new(instance_id.clone());
        db.instance_start(
            &instance_id,
            mode.as_str(),
            parent_instance.as_deref(),
            &task,
            &model,
            depth,
        )?;
        events.instance_start(
            &workspace.display().to_string(),
            mode.as_str(),
            &model,
            parent_instance.as_deref(),
            depth,
            &task,
        );
        Ok(Agent {
            cfg,
            workspace,
            db,
            instance_id,
            mode,
            llm,
            context,
            tokens_used: 0,
            token_limit: limits.token_limit,
            started: Instant::now(),
            depth,
            seq: 0,
            formal,
            active_plan: None,
            control,
            events,
            failed: false,
        })
    }

    pub fn resolve_path(&self, path: &str) -> PathBuf {
        crate::storage::util::resolve_path(&self.workspace, path)
    }

    pub fn display_path(&self, path: &Path) -> String {
        crate::storage::util::relative_path(&self.workspace, path)
    }

    /// The system prompt plus the active-plan section. Plan mode updates the
    /// plan (creating it when missing); other modes follow an existing plan and
    /// ignore a missing one.
    fn compose_system(&self) -> String {
        let mut system = build_system(&self.cfg, &self.workspace, self.mode, self.formal);
        let Some(slug) = &self.active_plan else {
            return system;
        };
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
            (false, false) => return system,
        };
        system.push_str(&format!(
            "\n\n## Active plan\n\nThe user selected plan `{slug}` at `{path}`. {guidance}"
        ));
        system
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
        )?;
        self.seq += 1;
        Ok(())
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
        let mut llm_retries = 0u32;
        loop {
            if let Some(reason) = self.budget_exceeded() {
                eprintln!("[budget] {reason}");
                self.events.error(&format!("stopped: {reason}"));
                return Ok(format!("(stopped: {reason})"));
            }
            let poll = self.poll_control();
            if poll.stop {
                eprintln!("[control] {STOPPED_BY_USER}");
                self.events.status(STOPPED_BY_USER);
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
            self.maybe_compact()?;
            let result = {
                let ctx = self.context.read().unwrap();
                self.llm.chat(ctx.messages(), ctx.tools())
            };
            let resp = match result {
                Ok(r) => r,
                Err(e) => return Ok(self.fail(format!("LLM request failed: {e:#}"))),
            };
            self.record_usage(resp.prompt_tokens, resp.completion_tokens);
            self.context
                .write()
                .unwrap()
                .set_last_prompt_tokens(resp.prompt_tokens);
            self.events
                .tokens(self.tokens_used, resp.prompt_tokens, resp.completion_tokens);
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
                continue;
            }
            llm_retries = 0;

            let assistant = resp.message;
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

            self.execute_tool_calls(&tool_calls, msg_seq)?;

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

    fn execute_tool_calls(&mut self, calls: &[llm::ToolCall], msg_seq: i64) -> Result<()> {
        for tc in calls {
            let start = Instant::now();
            let (result, is_error) = match serde_json::from_str::<Value>(&tc.arguments) {
                Ok(args) => tools::dispatch(self, &tc.name, &args),
                Err(e) => (format!("ERROR: invalid JSON tool arguments: {e}"), true),
            };
            let duration = i64::try_from(start.elapsed().as_millis()).unwrap_or(i64::MAX);
            self.db.tool_call_add(
                &self.instance_id,
                msg_seq,
                &tc.name,
                &tc.arguments,
                &result,
                is_error,
                duration,
            )?;
            self.events
                .tool_result(&tc.id, &tc.name, is_error, duration, &result);
            if is_error {
                eprintln!(
                    "[tool] {}({}) -> ERROR ({}ms)",
                    tc.name,
                    llm::truncate(&tc.arguments, 120),
                    duration
                );
            }
            self.log(ChatMessage::tool_result(&tc.id, result))?;
        }
        Ok(())
    }

    fn fail(&mut self, msg: String) -> String {
        eprintln!("[llm] {msg}");
        self.events.error(&msg);
        let _ = self.log(ChatMessage::assistant(&msg));
        self.failed = true;
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

    fn maybe_compact(&mut self) -> Result<()> {
        let compacted = self.context.write().unwrap().maybe_compact(
            self.cfg.compact_threshold,
            self.cfg.compact_keep_recent,
            &self.llm,
        )?;
        let Some(c) = compacted else {
            return Ok(());
        };
        self.record_usage(c.prompt_tokens, c.completion_tokens);
        self.db
            .compaction_add(&self.instance_id, c.removed, c.before, c.after, &c.summary)?;
        eprintln!(
            "[compact] removed {} messages ({} -> {} est tokens)",
            c.removed, c.before, c.after
        );
        self.events
            .compaction(c.removed, c.before, c.after, &c.summary);
        Ok(())
    }

    pub fn finish(&self, status: &str, report: &str) -> Result<()> {
        self.events.instance_end(status, self.tokens_used, report);
        self.db
            .instance_end(&self.instance_id, status, self.tokens_used, report)
    }
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
    if extended.trim().is_empty() {
        return base;
    }
    format!("{base}\n\n## Extended guidance\n{extended}")
}

#[cfg(test)]
mod tests {
    use super::build_system;
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
}
