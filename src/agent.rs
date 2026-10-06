use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use crate::config::{self, AgentDef, Config, Provider, Skill};
use crate::llm::{self, ChatMessage, LlmClient, Role, ToolCall};
use crate::socket::Control;
use crate::storage::context::ContextComposer;
use crate::storage::events::{self, EventEmitter};
use crate::storage::util::{relative_path, resolve_path};
use crate::tools::{self, Verdict};

const MAX_TRUNCATIONS: u32 = 3;
const LLM_TIMEOUT_SECS: u64 = 600;
const STOPPED_BY_USER: &str = "(stopped by user via control socket)";
const WRAP_UP_TURNS: usize = 5;
/// Fraction of the context window at which old turns are compacted.
const COMPACT_AT: f64 = 0.8;
/// Messages between prune checks, so one prune clears many turns.
/// Tools whose repeated identical output is replaced by a pointer to the earlier result.
const DEDUPED: [&str; 3] = ["read", "ls", "bash"];
const PRUNE_MIN_GAP: usize = 8;
/// A prune rewrites the cached prefix; it must free at least this many tokens...
const PRUNE_MIN_FREE_TOKENS: i64 = 2000;
/// ...or this fraction of the context, whichever is larger.
const PRUNE_MIN_FREE_FRACTION: f64 = 0.15;
/// Provider prompt caches expire after about this long idle, so the prefix is free to rewrite.
const CACHE_TTL: Duration = Duration::from_secs(300);
const CACHE_COLD_MIN_FREE_TOKENS: i64 = 500;
/// Past this fraction of the window, any stale content goes (before compaction is needed).
const PRUNE_PRESSURE: f64 = 0.5;
const CONTINUE: &str = "Continue from where you left off.";
const TRUNCATED_HINT: &str = "Your last response was cut off by the output limit. Continue with smaller steps: split large writes into several edits.";
const INTERRUPTED: &str = "ERROR: interrupted — genji stopped while this tool was running; it may have partially run. Verify the current state before retrying.";

/// How a run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Done,
    Failed,
    Stopped,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Done => "done",
            Status::Failed => "failed",
            Status::Stopped => "stopped",
        }
    }
}

/// Why a stopped run stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    TokenLimit,
    TimeLimit,
    MaxIterations,
    User,
}

impl StopReason {
    pub fn as_str(self) -> &'static str {
        match self {
            StopReason::TokenLimit => "token_limit",
            StopReason::TimeLimit => "time_limit",
            StopReason::MaxIterations => "max_iterations",
            StopReason::User => "user",
        }
    }
}

pub struct Agent {
    pub cfg: Config,
    pub workspace: PathBuf,
    pub def: AgentDef,
    pub agents: BTreeMap<String, AgentDef>,
    /// The agent that spawned this run; set only for subagents.
    pub parent_agent: Option<String>,
    pub instance_id: String,
    pub depth: u32,
    /// The id of the tool call being dispatched, as logged in the session.
    pub call_id: String,
    pub verdict: Option<Verdict>,
    /// Set by `hand_off`: genji continues with `verdict.next` instead of exiting.
    pub handed_off: bool,
    pub status: Status,
    pub reason: Option<StopReason>,
    pub llm: LlmClient,
    skills: BTreeMap<String, Skill>,
    context: Arc<RwLock<ContextComposer>>,
    pub control: Arc<Control>,
    events: EventEmitter,
    tokens_used: i64,
    token_limit: i64,
    started: Instant,
    /// (tool, raw args) -> (call id, result hash) of the latest read-only call, for dedupe.
    seen: HashMap<(String, String), (String, u64)>,
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
    pub control: Arc<Control>,
    /// Shared with the control socket; replaced by this run's context.
    pub context: Arc<RwLock<ContextComposer>>,
}

/// System prompt: the agent's prompt, environment, project instructions, skills, agents, reporting.
fn build_system(
    workspace: &Path,
    def: &AgentDef,
    agents: &BTreeMap<String, AgentDef>,
    skills: &BTreeMap<String, Skill>,
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
    let listed: Vec<_> = skills
        .values()
        .filter(|k| !k.disable_model_invocation && !def.skills.contains(&k.name))
        .collect();
    if has("read") && !listed.is_empty() {
        s.push_str(
            "\n\n## Skills\nWhen a task matches a skill's description, `read` its SKILL.md and follow it. \
             Paths inside a skill are relative to its directory.\n",
        );
        for k in listed {
            s.push_str(&format!(
                "- {} — {} ({})\n",
                k.name,
                k.description,
                k.path.display()
            ));
        }
    }
    for name in &def.skills {
        let Some(skill) = skills.get(name) else {
            let names: Vec<_> = skills.keys().cloned().collect();
            anyhow::bail!(
                "agent `{}` forces skill `{name}`, which does not exist. available: {}",
                def.name,
                names.join(", ")
            );
        };
        s.push_str(&format!("\n\n{}", config::render_skill(skill)));
    }
    if has("spawn") || has("hand_off") {
        s.push_str("\n\n## Agents\n");
        for a in agents.values().filter(|a| def.may_spawn(&a.name)) {
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

/// Drops timestamp keys (`created`, `updated`, ...) from a leading YAML frontmatter block;
/// they cost tokens and carry nothing the agent acts on.
fn strip_timestamps(text: &str) -> String {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text.to_string();
    };
    let Some(end) = rest.find("\n---") else {
        return text.to_string();
    };
    let (front, tail) = rest.split_at(end);
    let kept: Vec<_> = front
        .lines()
        .filter(|l| {
            let key = l.split(':').next().unwrap_or("").trim();
            !matches!(key, "created" | "updated" | "created_at" | "updated_at")
        })
        .collect();
    format!("---\n{}{tail}", kept.join("\n"))
}

/// The `context:` files of an agent definition, rendered for the first message: file contents
/// (capped), a listing for entries ending in `/`, and a note for the ones that do not exist yet.
fn project_docs(workspace: &Path, paths: &[String]) -> String {
    const MAX: usize = 8 * 1024;
    let mut out = String::from("# Project docs\n");
    for p in paths {
        let full = workspace.join(p);
        if p.ends_with('/') {
            let mut names: Vec<String> = std::fs::read_dir(&full)
                .map(|d| {
                    d.flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            if names.is_empty() {
                out.push_str(&format!("\n## {p}\n(empty or missing)\n"));
            } else {
                out.push_str(&format!("\n## {p}\n{}\n", names.join("\n")));
            }
            continue;
        }
        match std::fs::read_to_string(&full) {
            Ok(text) => {
                let text = strip_timestamps(&text);
                let body = llm::truncate(&text, MAX);
                out.push_str(&format!("\n## {p}\n{}\n", body.trim_end()));
            }
            Err(_) => out.push_str(&format!("\n## {p}\n({p} does not exist yet)\n")),
        }
    }
    out
}

/// The same read-only call returning the same output as an earlier call whose result is still
/// verbatim in context is answered with a pointer to it instead of a second copy.
fn dedupe(
    seen: &mut HashMap<(String, String), (String, u64)>,
    ctx: &ContextComposer,
    tc: &ToolCall,
    result: String,
) -> String {
    let hash = hash_of(&result);
    let key = (tc.name().to_string(), tc.args().to_string());
    if let Some((id, h)) = seen.get(&key) {
        let intact = ctx.tool_result(id).is_some_and(|c| hash_of(c) == *h);
        if *h == hash && intact {
            return format!("[unchanged since call {id}: identical output is above in context]");
        }
    }
    seen.insert(key, (tc.id.clone(), hash));
    result
}

fn hash_of(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Pruning trades tokens freed against rewriting the cached prefix: prune when the gain is large
/// relative to the context, when the cache has expired anyway, or when the window is filling.
fn should_prune(freed: i64, est_tokens: i64, window: i64, idle: Duration) -> bool {
    if freed <= 0 {
        return false;
    }
    let worth_rewrite =
        freed >= PRUNE_MIN_FREE_TOKENS.max((est_tokens as f64 * PRUNE_MIN_FREE_FRACTION) as i64);
    let cache_cold = idle >= CACHE_TTL && freed >= CACHE_COLD_MIN_FREE_TOKENS;
    let pressure = est_tokens as f64 >= window as f64 * PRUNE_PRESSURE;
    worth_rewrite || cache_cold || pressure
}

const CONTEXT_OVERFLOW_HINTS: [&str; 5] = [
    "context_length",
    "context length",
    "maximum context",
    "too many tokens",
    "exceeds the context",
];

fn is_context_overflow(err: &str) -> bool {
    let e = err.to_lowercase();
    CONTEXT_OVERFLOW_HINTS.iter().any(|k| e.contains(k))
}

/// The state `Agent::start` begins from: a fresh run, or one replayed from its session.
struct StartState {
    model: String,
    seq: u64,
    tokens_used: i64,
    pending: Vec<ToolCall>,
    recorded: Option<ContextComposer>,
}

impl StartState {
    fn fresh(def: &AgentDef, provider: &Provider) -> Self {
        Self {
            model: def.model.clone().unwrap_or_else(|| provider.model.clone()),
            seq: 0,
            tokens_used: 0,
            pending: Vec::new(),
            recorded: None,
        }
    }

    fn replayed(session: &Path, window: i64) -> Result<Self> {
        let r = events::replay(session, window)?;
        Ok(Self {
            model: r.model,
            seq: r.seq,
            tokens_used: r.tokens_used,
            pending: r.pending,
            recorded: Some(r.ctx),
        })
    }
}

/// Bookkeeping for one `run_loop`: progress counters and the one-request hint.
struct RunState {
    iterations: usize,
    truncations: u32,
    overflow_retried: bool,
    nudged: bool,
    pruned_at: usize,
    last_call: Instant,
    hint: Option<String>,
}

impl RunState {
    fn new(context_len: usize) -> Self {
        Self {
            iterations: 0,
            truncations: 0,
            overflow_retried: false,
            nudged: false,
            pruned_at: context_len,
            last_call: Instant::now(),
            hint: None,
        }
    }
}

impl Agent {
    /// Start a run, or with `resume` continue the recorded one: its system prompt,
    /// tools, model and context come from the session file so the request is the
    /// same prefix the provider already cached.
    pub fn start(p: AgentParams) -> Result<Self> {
        let session = p
            .cfg
            .sessions(&p.workspace)
            .join(format!("{}.jsonl", p.instance_id));
        let window = p.provider.context_window;
        let token_limit = p.cfg.token_limit(&p.provider);
        let start = if p.resume {
            StartState::replayed(&session, window)?
        } else {
            StartState::fresh(&p.def, &p.provider)
        };
        let llm = LlmClient::new(
            p.provider,
            start.model.clone(),
            p.cfg.llm_max_retries,
            LLM_TIMEOUT_SECS,
        );
        let events =
            EventEmitter::open(&p.instance_id, &session, start.seq)?.with_tap(p.control.clone());
        events.instance_start(
            &p.workspace.display().to_string(),
            &p.def.name,
            &start.model,
            p.parent.as_deref(),
            p.depth,
            &p.task,
            p.resume,
        );
        let skills = config::load_skills(&p.cfg.skills(&p.workspace));
        let ctx = match start.recorded {
            Some(ctx) => ctx,
            None => {
                let system = build_system(
                    &p.workspace,
                    &p.def,
                    &p.agents,
                    &skills,
                    p.parent_agent.as_deref(),
                )?;
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
            call_id: String::new(),
            verdict: None,
            handed_off: false,
            status: Status::Done,
            reason: None,
            llm,
            skills,
            context: p.context,
            control: p.control,
            events,
            tokens_used: start.tokens_used,
            token_limit,
            started: Instant::now(),
            seen: HashMap::new(),
        };
        // `spawn`, `finish`, `hand_off` and `verdict` are safe to run again; any other call may have partly run.
        for tc in start.pending {
            if matches!(tc.name(), "spawn" | "finish" | "hand_off" | "verdict") {
                agent.run_call(&tc);
            } else {
                agent.log_result(&tc, INTERRUPTED.to_string(), true, Duration::ZERO);
            }
        }
        Ok(agent)
    }

    pub fn resolve_path(&self, path: &str) -> PathBuf {
        resolve_path(&self.workspace, path)
    }

    /// Block the current tool call on a human answer sent as `/answer <call_id> <json string>`.
    /// A value outside `options` is free text. After `ask_timeout_secs` without an
    /// answer the recommended option is used.
    pub fn ask(&self, options: &[String], recommended: &str) -> Result<String> {
        let secs = self.cfg.ask_timeout_secs;
        match self
            .control
            .wait_answer(&self.call_id, Duration::from_secs(secs))
        {
            Some(v) if options.contains(&v) => Ok(format!("answer: {v}")),
            Some(v) => Ok(format!("answer (free text): {v}")),
            None if self.control.stop_requested() => {
                anyhow::bail!("stopped by user while waiting for an answer")
            }
            None => Ok(format!(
                "answer: {recommended} (no reply within {secs}s; recommended option used)"
            )),
        }
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
        self.context
            .write()
            .unwrap()
            .push(ChatMessage::tool_result(&tc.id, result));
    }

    fn run_call(&mut self, tc: &ToolCall) {
        self.call_id = tc.id.clone();
        let start = Instant::now();
        let (mut result, is_error) = match serde_json::from_str::<Value>(tc.args()) {
            Ok(args) => tools::dispatch(self, tc.name(), &args),
            Err(e) => (format!("ERROR: invalid JSON tool arguments: {e}"), true),
        };
        if !is_error && DEDUPED.contains(&tc.name()) {
            let ctx = self.context.read().unwrap();
            result = dedupe(&mut self.seen, &ctx, tc, result);
        }
        self.log_result(tc, result, is_error, start.elapsed());
    }

    /// The agent's `context:` files, then those of the skills it forces, without repeats.
    fn context_paths(&self) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let forced = self.def.skills.iter().filter_map(|n| self.skills.get(n));
        self.def
            .context
            .iter()
            .chain(forced.flat_map(|k| &k.context))
            .filter(|p| seen.insert((*p).clone()))
            .cloned()
            .collect()
    }

    /// Run until the agent finishes or stops, then record `instance_end`.
    /// `task` is appended as a user message; a finished session resumed without
    /// one is told to continue.
    pub fn run(&mut self, task: Option<&str>) -> Result<String> {
        let (last, first_turn) = {
            let ctx = self.context.read().unwrap();
            (ctx.messages().last().map(|m| m.role), ctx.len() == 1)
        };
        let paths = if first_turn {
            self.context_paths()
        } else {
            Vec::new()
        };
        match (task, last) {
            (Some(t), _) if !paths.is_empty() => {
                let docs = project_docs(&self.workspace, &paths);
                self.log(ChatMessage::user(format!("{docs}\n# Task\n{t}")));
            }
            (Some(t), _) => self.log(ChatMessage::user(t)),
            (None, Some(Role::Assistant)) => self.log(ChatMessage::user(CONTINUE)),
            _ => {}
        }
        let report = self
            .run_loop()
            .unwrap_or_else(|e| self.fail(format!("{e:#}")));
        let result = self.verdict.as_ref().map(|v| json!(v));
        self.events.instance_end(
            self.status.as_str(),
            self.reason.map(StopReason::as_str),
            self.tokens_used,
            &report,
            result.as_ref(),
        );
        Ok(report)
    }

    fn fail(&mut self, msg: String) -> String {
        eprintln!("[llm] {msg}");
        self.events.error(&msg);
        self.status = Status::Failed;
        msg
    }

    fn stop(&mut self, reason: StopReason, msg: String) -> String {
        self.events.error(&msg);
        self.status = Status::Stopped;
        self.reason = Some(reason);
        format!("({msg})")
    }

    fn stopped_by_user(&mut self) -> String {
        self.events.status(STOPPED_BY_USER);
        self.status = Status::Stopped;
        self.reason = Some(StopReason::User);
        STOPPED_BY_USER.to_string()
    }

    fn budget_exceeded(&self) -> Option<(StopReason, String)> {
        if self.tokens_used >= self.token_limit {
            return Some((
                StopReason::TokenLimit,
                format!(
                    "token limit reached ({} >= {})",
                    self.tokens_used, self.token_limit
                ),
            ));
        }
        if self.started.elapsed().as_secs() >= self.cfg.time_limit_secs.max(1) {
            return Some((
                StopReason::TimeLimit,
                format!("time limit reached ({}s)", self.cfg.time_limit_secs),
            ));
        }
        None
    }

    fn run_loop(&mut self) -> Result<String> {
        let mut state = RunState::new(self.context.read().unwrap().len());
        let ends: Vec<&str> = ["hand_off", "finish", "verdict"]
            .into_iter()
            .filter(|t| self.def.tools.iter().any(|n| n == t))
            .collect();
        let ends = ends.join(" or ");
        loop {
            if let Some((reason, msg)) = self.budget_exceeded() {
                return Ok(self.stop(reason, msg));
            }
            let (stop, _) = self.poll_control();
            if stop {
                return Ok(self.stopped_by_user());
            }
            self.set_status();
            self.maybe_compact(COMPACT_AT, true)?;
            self.maybe_prune(&mut state);
            if state.hint.is_none()
                && !ends.is_empty()
                && state.iterations + WRAP_UP_TURNS >= self.cfg.max_tool_iterations
            {
                // Sent for the last few tool-call turns so the run ends with a report, not a cut-off.
                state.hint = Some(format!(
                    "You are about to run out of tool calls and will be cut off. Wrap up now: stop starting new work, then call {ends} with a standalone report of what is done, what remains, and where the state lives."
                ));
            }
            let result = {
                let ctx = self.context.read().unwrap();
                self.llm
                    .chat(ctx.messages(), ctx.tools(), state.hint.take().as_deref())
            };
            state.last_call = Instant::now();
            let resp = match result {
                Ok(r) => r,
                Err(e) if !state.overflow_retried && is_context_overflow(&format!("{e:#}")) => {
                    state.overflow_retried = true;
                    self.events
                        .error("context overflow; compacting and retrying");
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
                if state.truncations >= MAX_TRUNCATIONS {
                    return Ok(self.fail(format!(
                        "LLM request failed: response truncated {MAX_TRUNCATIONS} times"
                    )));
                }
                state.truncations += 1;
                self.events.error(&format!(
                    "LLM response truncated (finish_reason=length); retry {}/{MAX_TRUNCATIONS}",
                    state.truncations
                ));
                state.hint = Some(TRUNCATED_HINT.into());
                continue;
            }
            state.truncations = 0;
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
                    return Ok(self.stopped_by_user());
                }
                if injected {
                    continue;
                }
                if !state.nudged && !ends.is_empty() {
                    state.nudged = true;
                    state.hint = Some(format!("End by calling {ends}."));
                    continue;
                }
                return Ok(text);
            }
            state.nudged = false;
            let mut calls = assistant.tool_calls.clone();
            self.log(assistant);
            // `finish` and `hand_off` run after the other calls of its turn.
            calls.sort_by_key(|c| matches!(c.name(), "finish" | "hand_off"));
            for tc in &calls {
                self.run_call(tc);
            }
            if let Some(v) = &self.verdict {
                return Ok(v.summary.clone());
            }
            state.iterations += 1;
            if state.iterations >= self.cfg.max_tool_iterations {
                return Ok(self.stop(
                    StopReason::MaxIterations,
                    format!(
                        "reached max tool iterations {}",
                        self.cfg.max_tool_iterations
                    ),
                ));
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
        self.control.set_status(status.clone());
        self.events.status(&status);
    }

    /// Queued user instructions enter the context. Returns (stop requested, injected).
    /// A review pass leaves them queued: instructions belong to the work pass.
    fn poll_control(&mut self) -> (bool, bool) {
        let ctrl = self.control.clone();
        if self.def.worker() != self.def.name {
            return (ctrl.stop_requested(), false);
        }
        let queued = ctrl.drain();
        for ins in &queued {
            self.log(ChatMessage::user(format!("{}{ins}", events::INSTRUCTION)));
        }
        (ctrl.stop_requested(), !queued.is_empty())
    }

    /// Drops what the model no longer needs, but only when it pays for rewriting the provider's
    /// cached prefix (see `should_prune`); checked every `PRUNE_MIN_GAP` new messages.
    fn maybe_prune(&mut self, state: &mut RunState) {
        let len = self.context.read().unwrap().len();
        if len < state.pruned_at {
            state.pruned_at = len;
        }
        if len - state.pruned_at < PRUNE_MIN_GAP {
            return;
        }
        state.pruned_at = len;
        let keep = self.cfg.prune_keep_recent;
        let idle = state.last_call.elapsed();
        // Old results stay verbatim (cache and facts intact) until the window fills.
        let bulk = self.context.read().unwrap().over(PRUNE_PRESSURE);
        let pruned = self
            .context
            .write()
            .unwrap()
            .prune_if(keep, bulk, |freed, est, window| {
                should_prune(freed, est, window, idle)
            });
        if pruned {
            self.events.prune(keep, bulk);
        }
    }

    /// Prune old tool results (gently, then tightly), then summarize older turns, once the context nears the window.
    /// Both are logged so a resume rebuilds the same context.
    fn maybe_compact(&mut self, threshold: f64, optional: bool) -> Result<()> {
        let keep = self.cfg.compact_keep_recent;
        if !self.context.read().unwrap().over(threshold) {
            return Ok(());
        }
        // Gentle prune first; the tight one only if that was not enough.
        for keep in [self.cfg.prune_keep_recent.max(keep), keep] {
            if self.context.write().unwrap().prune(keep, true) {
                self.events.prune(keep, true);
                if !self.context.read().unwrap().over(threshold) {
                    return Ok(());
                }
            }
        }
        let Some((rendered, kept)) = self.context.read().unwrap().compaction_source(keep) else {
            return Ok(());
        };
        let req = [
            ChatMessage::system(
                "You compress an agent's running history so it can continue the work. Be dense and factual. Use these sections: Goal; Decisions and why; Files created or changed (paths); Commands run and their outcome (passing/failing tests, errors); Open problems; Next step. Keep exact identifiers, paths, ids and error messages. Drop pleasantries, file contents that are on disk, and anything recoverable by reading the workspace.",
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::util::temp_dir;

    #[test]
    fn project_docs_render_files_listings_and_missing() {
        let ws = temp_dir("docs");
        std::fs::create_dir_all(ws.join("docs/adr")).unwrap();
        std::fs::write(ws.join("docs/architecture.md"), "- src/a.rs — a\n").unwrap();
        std::fs::write(ws.join("docs/adr/orm.md"), "x").unwrap();
        let out = project_docs(
            &ws,
            &[
                "docs/architecture.md".into(),
                "docs/domain-model.md".into(),
                "docs/adr/".into(),
            ],
        );
        assert!(
            out.contains("## docs/architecture.md\n- src/a.rs — a\n"),
            "{out}"
        );
        assert!(
            out.contains("(docs/domain-model.md does not exist yet)"),
            "{out}"
        );
        assert!(out.contains("## docs/adr/\norm.md"), "{out}");
    }

    #[test]
    fn pinned_files_drop_timestamp_frontmatter() {
        let t = "---\nid: R1\ncreated: 2026-01-01\nstatus: open\nupdated: 2026-02-02\n---\nbody created: x\n";
        assert_eq!(
            strip_timestamps(t),
            "---\nid: R1\nstatus: open\n---\nbody created: x\n"
        );
        assert_eq!(strip_timestamps("no front\n"), "no front\n");
    }

    #[test]
    fn repeated_output_still_in_context_is_deduped() {
        let mut seen = HashMap::new();
        let mut ctx = ContextComposer::new("sys".into(), vec![], 1000);
        let call = |id: &str| ToolCall::new(id, "bash", r#"{"command":"cat a"}"#.to_string());
        let mut run = |ctx: &mut ContextComposer, id: &str, out: &str| {
            let r = dedupe(&mut seen, ctx, &call(id), out.to_string());
            ctx.push(ChatMessage::tool_result(id, r.clone()));
            r
        };
        assert_eq!(run(&mut ctx, "c1", "body"), "body");
        assert!(run(&mut ctx, "c2", "body").contains("unchanged since call c1"));
        assert_eq!(run(&mut ctx, "c3", "changed"), "changed");
        assert!(run(&mut ctx, "c4", "changed").contains("call c3"));
        // An earlier result that was elided no longer counts.
        let big = "x".repeat(5000);
        assert_eq!(run(&mut ctx, "c5", &big), big);
        for i in 0..4 {
            ctx.push(ChatMessage::user(format!("m{i}")));
        }
        assert!(ctx.prune(2, true));
        assert_eq!(run(&mut ctx, "c6", &big), big);
    }

    #[test]
    fn prune_gate_weighs_gain_against_cache_rewrite() {
        let hot = Duration::from_secs(5);
        let cold = CACHE_TTL;
        assert!(!should_prune(0, 100_000, 350_000, cold));
        assert!(!should_prune(1000, 100_000, 350_000, hot));
        assert!(!should_prune(10_000, 100_000, 350_000, hot));
        assert!(should_prune(15_000, 100_000, 350_000, hot));
        assert!(should_prune(600, 100_000, 350_000, cold));
        assert!(!should_prune(400, 100_000, 350_000, cold));
        assert!(should_prune(10, 200_000, 350_000, hot));
    }

    #[test]
    fn hints_carry_no_note_prefix_of_their_own() {
        // `LlmClient::chat` adds the `[note]` prefix.
        assert!(!TRUNCATED_HINT.contains("[note]"));
    }

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
        let skill_dir = ws.join(".agents/skills/formal");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: formal\ndescription: tracks work\n---\nbody\n",
        )
        .unwrap();
        config::tests::write_agents(
            &ws,
            &[
                (
                    "lead",
                    "---\ndescription: leads\ntools: read, spawn, finish\nspawns: scout\n---\nl",
                ),
                (
                    "worker",
                    "---\ndescription: works\ntools: read, finish\n---\nw",
                ),
                (
                    "scout",
                    "---\ndescription: scouts\ntools: read, finish\n---\ns",
                ),
            ],
        );
        let agents = config::load_agents(&ws);
        let skills = config::load_skills(&Config::default().skills(&ws));
        let plan = build_system(&ws, &agents["lead"], &agents, &skills, None).unwrap();
        assert!(
            plan.contains("## Environment")
                && plan.contains("## Agents")
                && plan.contains("- scout — scouts")
                && !plan.contains("- worker — works")
        );
        let worker = build_system(&ws, &agents["worker"], &agents, &skills, None).unwrap();
        assert!(
            !worker.contains("## Agents"),
            "no spawn tool, no agent list"
        );
        assert!(
            plan.contains("## Skills")
                && plan.contains("- formal — ")
                && plan.contains(".agents/skills/formal/SKILL.md)")
                && !plan.contains("# Skill: formal")
        );
        assert!(!plan.contains("## Reporting"));
        let sub = build_system(&ws, &agents["scout"], &agents, &skills, Some("lead")).unwrap();
        assert!(sub.contains("## Reporting") && sub.contains("`next.agent` = `lead`"));
        let mut forced = agents["worker"].clone();
        forced.skills = vec!["formal".into()];
        let text = build_system(&ws, &forced, &agents, &skills, None).unwrap();
        assert!(text.contains("# Skill: formal") && !text.contains("- formal — "));
        forced.skills = vec!["nope".into()];
        assert!(build_system(&ws, &forced, &agents, &skills, None).is_err());
    }
}
