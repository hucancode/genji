mod agent;
mod config;
mod llm;
mod socket;
mod storage;
mod tools;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use agent::{Agent, AgentParams, Status, StopReason};
use config::{AgentDef, Config, dot};
use storage::context::ContextComposer;
use storage::events;

const USAGE: &str = "usage:
  genji <agent> [task] [--resume ID] [--parent ID] [--workspace DIR] [--provider P]
                [--socket PATH | --socket-disabled] [--sessions-dir DIR] [--token-limit N]
                [--config FILE] [--config-json JSON] [--agents-dir DIR]
  genji init [agent...] [--force] [--workspace DIR]
  genji help [agent|tool] [--json] | --version

control (while running): nc -U .genji/control.sock, then /help; or JSON lines on stdin";

#[derive(Default)]
struct Opts {
    positional: Vec<String>,
    workspace: Option<String>,
    provider: Option<String>,
    resume: Option<String>,
    parent: Option<String>,
    parent_agent: Option<String>,
    instance_id: Option<String>,
    instructions_file: Option<String>,
    config_json: Option<String>,
    config: Option<String>,
    agents_dir: Option<String>,
    socket: Option<String>,
    socket_disabled: bool,
    sessions_dir: Option<String>,
    token_limit: Option<i64>,
    depth: u32,
    json: bool,
    subagent: bool,
    force: bool,
    help: bool,
    version: bool,
}

fn parse(args: Vec<String>) -> Result<Opts> {
    let mut o = Opts::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = || -> Result<String> {
            inline
                .clone()
                .or_else(|| it.next())
                .with_context(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--workspace" => o.workspace = Some(value()?),
            "--provider" => o.provider = Some(value()?),
            "--resume" => o.resume = Some(value()?),
            "--parent" => o.parent = Some(value()?),
            "--parent-agent" => o.parent_agent = Some(value()?),
            "--instance-id" => o.instance_id = Some(value()?),
            "--instructions-file" => o.instructions_file = Some(value()?),
            "--config-json" => o.config_json = Some(value()?),
            "--config" => o.config = Some(value()?),
            "--agents-dir" => o.agents_dir = Some(value()?),
            "--socket" => o.socket = Some(value()?),
            "--socket-disabled" => o.socket_disabled = true,
            "--sessions-dir" => o.sessions_dir = Some(value()?),
            "--token-limit" => {
                o.token_limit = Some(value()?.parse().context("--token-limit needs a number")?)
            }
            "--depth" => o.depth = value()?.parse().context("--depth needs a number")?,
            "--subagent" => o.subagent = true,
            "--json" => o.json = true,
            "--force" => o.force = true,
            "--help" | "-h" => o.help = true,
            "--version" | "-V" => o.version = true,
            f if f.starts_with("--") => bail!("unknown option `{f}`"),
            _ => o.positional.push(arg),
        }
    }
    Ok(o)
}

/// The agents `genji` would load for a run: the effective config's `agents_dir`,
/// else the workspace's `.genji/agents`.
fn load_agents(o: &Opts) -> Result<BTreeMap<String, AgentDef>> {
    let ws = workspace(o)?;
    let cfg = resolve_config(o, &ws, false)?;
    Ok(config::load_agents_from(&cfg.agents(&ws)))
}

/// The effective config: `--config-json`, else `--config`, else the workspace's
/// `.genji/config.json`. `create` writes the default file when it is missing.
fn resolve_config(o: &Opts, ws: &Path, create: bool) -> Result<Config> {
    let path = o.config.as_deref().map(absolute).transpose()?;
    let mut cfg = match &o.config_json {
        Some(j) => Config::from_json(j)?,
        None if create => Config::load_or_create(ws, path.as_deref())?,
        None => Config::load(ws, path.as_deref())?,
    };
    if let Some(d) = &o.sessions_dir {
        cfg.sessions_dir = Some(absolute(d)?);
    }
    if let Some(d) = &o.agents_dir {
        cfg.agents_dir = Some(absolute(d)?);
    }
    if let Some(n) = o.token_limit {
        cfg.token_limit = n;
    }
    Ok(cfg)
}

fn workspace(o: &Opts) -> Result<PathBuf> {
    let w = match &o.workspace {
        Some(w) => PathBuf::from(w),
        None => std::env::current_dir()?,
    };
    Ok(std::fs::canonicalize(&w).unwrap_or(w))
}

fn absolute(p: &str) -> Result<PathBuf> {
    let p = PathBuf::from(p);
    Ok(if p.is_absolute() {
        p
    } else {
        std::env::current_dir()?.join(p)
    })
}

/// `genji help agent|tool [--json]`: loaded agents or the tool registry.
fn cmd_help(o: &Opts, topic: Option<&str>) -> Result<i32> {
    let agents = load_agents(o)?;
    match topic {
        Some("agent") if o.json => {
            let arr: Vec<Value> = agents.values().map(AgentDef::info).collect();
            println!("{}", serde_json::to_string(&arr)?);
        }
        Some("tool") if o.json => println!("{}", serde_json::to_string(&tools::list())?),
        Some("tool") => {
            for t in tools::list() {
                println!(
                    "  {:<11} {}",
                    t["name"].as_str().unwrap_or_default(),
                    t["description"].as_str().unwrap_or_default()
                );
            }
        }
        None | Some("agent") => println!("{}", usage(&agents)),
        Some(t) => {
            eprintln!("unknown help topic `{t}` (agent, tool)");
            return Ok(2);
        }
    }
    Ok(0)
}

fn usage(agents: &BTreeMap<String, AgentDef>) -> String {
    let mut out = format!("{USAGE}\n\nagents:");
    if agents.is_empty() {
        out.push_str("\n  (none; `genji init` writes the default agents to .genji/agents)");
    }
    for a in agents.values().filter(|a| !a.internal) {
        out.push_str(&format!("\n  {:<10} {}", a.name, a.description));
    }
    out
}

fn exit_code(status: Status) -> i32 {
    match status {
        Status::Done => 0,
        Status::Failed => 1,
        Status::Stopped => 2,
    }
}

fn read_task(file: Option<&str>, words: &[String]) -> Result<Option<String>> {
    if let Some(f) = file {
        let text = if f == "-" {
            std::io::read_to_string(std::io::stdin())
        } else {
            std::fs::read_to_string(f)
        }
        .with_context(|| format!("reading instructions file {f}"))?;
        if !text.trim().is_empty() {
            return Ok(Some(text));
        }
    }
    let task = words.join(" ");
    Ok((!task.trim().is_empty()).then_some(task))
}

/// Keeps the control socket alive; removes it on drop.
struct Guard {
    control: Arc<socket::Control>,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.control.shutdown();
    }
}

fn start_control(
    socket_path: Option<PathBuf>,
    context: Arc<RwLock<ContextComposer>>,
) -> Result<Guard> {
    let control = socket::Control::open(socket_path, context)?;
    // Machines drive a run through stdin; a terminal is for the socket.
    if !std::io::stdin().is_terminal() {
        control.read_commands(std::io::BufReader::new(std::io::stdin()));
    }
    Ok(Guard { control })
}

fn run_agent(o: Opts) -> Result<i32> {
    let ws = workspace(&o)?;
    let mut cfg = resolve_config(&o, &ws, true)?;
    if cfg.agents_dir.is_none() {
        config::ensure_agents(&ws)?;
    }
    let agents = config::load_agents_from(&cfg.agents(&ws));
    let sessions = cfg.sessions(&ws);
    let resume = match &o.resume {
        Some(prefix) => Some(resume_target(&sessions, prefix)?),
        None => None,
    };
    let name = match (o.positional.first(), &resume) {
        (Some(n), _) => n.clone(),
        (None, Some((_, a, _))) => a.clone(),
        (None, None) => {
            eprintln!("{}", usage(&agents));
            return Ok(2);
        }
    };
    // A resumed review pass is rebuilt from its worker's definition.
    let Some(mut def) = find_agent(&agents, &name) else {
        eprintln!("unknown agent or command `{name}`\n\n{}", usage(&agents));
        return Ok(2);
    };
    if let Some(p) = o
        .provider
        .clone()
        .or_else(|| std::env::var("GENJI_PROVIDER").ok())
        .filter(|p| !p.is_empty())
    {
        cfg.provider = p;
    }
    let mut provider = cfg.provider()?;
    let words = o.positional.get(1..).unwrap_or_default();
    let mut task = read_task(o.instructions_file.as_deref(), words)?;
    let first_id = match (&resume, &o.instance_id) {
        (Some((id, ..)), _) => id.clone(),
        (None, Some(id)) => id.clone(),
        (None, None) => storage::util::new_id(),
    };
    let model = def.model.clone().unwrap_or_else(|| provider.model.clone());
    let window = effective_window(
        cfg.preferred_context_size,
        provider.context_window,
        llm::model_context_window(&provider, &model),
    );
    provider.context_window = window;
    // Every run listens on a control socket for humans (unless disabled) and reads commands
    // from stdin for machines.
    let context = Arc::new(RwLock::new(ContextComposer::new(
        String::new(),
        Vec::new(),
        provider.context_window,
    )));
    if o.socket.is_some() && !cfg!(feature = "socket") {
        bail!("--socket needs a genji built with the `socket` feature");
    }
    let socket_path = if cfg!(feature = "socket") && !o.socket_disabled {
        Some(match &o.socket {
            Some(p) => absolute(p)?,
            None => dot(&ws, "control.sock"),
        })
    } else {
        None
    };
    let guard = start_control(socket_path, context.clone())?;
    // A resumed subagent continues where it stopped; anything else without a task waits for one.
    if task.is_none() && !(o.subagent && resume.is_some()) {
        guard.control.set_status("idle");
        match guard.control.wait_for_instruction() {
            Some(queued) => task = Some(queued.join("\n")),
            None => return Ok(0),
        }
    }
    let mut id = first_id;
    let mut parent = o
        .parent
        .clone()
        .or_else(|| resume.as_ref().and_then(|(_, _, p)| p.clone()));
    let mut resuming = resume.is_some();
    loop {
        let mut a = Agent::start(AgentParams {
            cfg: cfg.clone(),
            workspace: ws.clone(),
            def: def.clone(),
            agents: agents.clone(),
            provider: provider.clone(),
            instance_id: id.clone(),
            parent: parent.clone(),
            parent_agent: o.parent_agent.clone().filter(|_| o.subagent),
            depth: o.depth,
            resume: resuming,
            task: task.clone().unwrap_or_default(),
            control: guard.control.clone(),
            context: context.clone(),
        })?;
        a.run(task.as_deref())?;
        // Two passes: a top-level work pass that submits or runs out of tool iterations is
        // judged by a review pass with a fresh context.
        let submitted = (a.verdict.as_ref().is_some_and(|v| v.status == "done")
            || (a.status == Status::Stopped && a.reason == Some(StopReason::MaxIterations)))
            && !o.subagent
            && context.read().unwrap().peaked_over(cfg.review_threshold);
        match next_step(&a, &def, &guard.control, task.as_deref(), submitted) {
            Step::Review(reviewer) => {
                task = Some(events::review_input(&sessions.join(format!("{id}.jsonl")))?);
                let mut n = 1;
                while sessions.join(format!("{id}-review-{n}.jsonl")).exists() {
                    n += 1;
                }
                parent = Some(id.clone());
                id = format!("{id}-review-{n}");
                def = reviewer;
                resuming = false;
            }
            Step::BackToWork(back) => {
                // The work instance continues in its own context: its id is the review's parent.
                id = parent.clone().unwrap_or_default();
                let s = events::summary(&sessions.join(format!("{id}.jsonl")))?;
                parent = s["parent"].as_str().map(String::from);
                def = agents[def.worker()].clone();
                task = back;
                resuming = true;
            }
            Step::Next(next) => {
                parent = Some(id.clone());
                id = storage::util::new_id();
                def = agents[&next.agent].clone();
                task = Some(next.task);
                resuming = false;
            }
            Step::Exit => return Ok(exit_code(a.status)),
        }
    }
}

/// The `--resume` target: `(instance id, agent name, parent)`.
fn resume_target(sessions: &Path, prefix: &str) -> Result<(String, String, Option<String>)> {
    let s = events::summary(&events::find_session(sessions, prefix)?)?;
    Ok((
        s["id"].as_str().unwrap_or_default().to_string(),
        s["agent"].as_str().unwrap_or_default().to_string(),
        s["parent"].as_str().map(String::from),
    ))
}

/// The definition for `name`, or a review pass rebuilt from its worker's definition.
fn find_agent(agents: &BTreeMap<String, AgentDef>, name: &str) -> Option<AgentDef> {
    agents.get(name).cloned().or_else(|| {
        name.strip_suffix(config::REVIEW_SUFFIX)
            .and_then(|w| agents.get(w))
            .and_then(AgentDef::reviewer)
    })
}

/// What follows one instance: start its review, resume the work instance, continue with a
/// fresh instance, or stop.
enum Step {
    Review(AgentDef),
    /// The work instance continues with the findings of a `reject`; `None` when a `done` review
    /// left user instructions queued for it to drain.
    BackToWork(Option<String>),
    Next(tools::Next),
    Exit,
}

/// The transition after `a` ends, in priority order: review the work pass, send a review's
/// findings back, continue the chain, or stop.
fn next_step(
    a: &Agent,
    def: &AgentDef,
    control: &socket::Control,
    task: Option<&str>,
    submitted: bool,
) -> Step {
    if let Some(reviewer) = def.reviewer().filter(|_| submitted) {
        return Step::Review(reviewer);
    }
    if def.worker() != def.name {
        match a.verdict.as_ref() {
            Some(v) if v.status == "reject" => {
                return Step::BackToWork(Some(format!("{}{}", events::REJECTED, v.summary)));
            }
            Some(v) if v.status == "done" && control.pending() => return Step::BackToWork(None),
            _ => {}
        }
    }
    match next_instance(a, task) {
        Some(next) => Step::Next(next),
        None => Step::Exit,
    }
}

/// The instance that continues the chain: a `hand_off` target, or for an agent that can hand
/// off and ran out of tool iterations a fresh instance of itself (the shared run limits, time
/// and tokens, still bound the chain).
fn next_instance(a: &Agent, task: Option<&str>) -> Option<tools::Next> {
    // A `hand_off` continues in this process as a new instance of `next.agent`:
    // new id, new session file, fresh context.
    let next = a
        .verdict
        .as_ref()
        .filter(|_| a.handed_off && a.status == Status::Done)
        .and_then(|v| v.next.clone());
    next.or_else(|| {
        (a.status == Status::Stopped
            && a.reason == Some(StopReason::MaxIterations)
            && a.def.tools.iter().any(|t| t == "hand_off"))
        .then(|| tools::Next {
            agent: a.def.name.clone(),
            task: {
                const NOTE: &str = "\n\nA previous instance used up its tool iterations on this task. Inspect the workspace and the task's tracking files for what is already done, then continue with what remains.";
                let t = task.unwrap_or_default();
                if t.ends_with(NOTE) { t.to_string() } else { format!("{t}{NOTE}") }
            },
        })
    })
}

/// The smallest non-zero of the project's preferred size, the provider's window and the
/// server-reported model window.
fn effective_window(project: i64, provider: i64, model: Option<i64>) -> i64 {
    [project, provider, model.unwrap_or(0)]
        .into_iter()
        .filter(|n| *n > 0)
        .min()
        .unwrap_or(0)
}

/// `genji init [agent...] [--force]`: writes the default agent files; prints
/// `{"written":[...],"skipped":[...]}`.
fn cmd_init(o: &Opts, only: &[String]) -> Result<()> {
    let r = config::init_agents(&workspace(o)?, o.force, only)?;
    println!(
        "{}",
        serde_json::to_string(&json!({ "written": r.written, "skipped": r.skipped }))?
    );
    Ok(())
}

fn real_main() -> Result<i32> {
    let o = parse(std::env::args().skip(1).collect())?;
    if o.version {
        println!("genji {}", env!("CARGO_PKG_VERSION"));
        return Ok(0);
    }
    let first = o.positional.first().map(String::as_str);
    if first == Some("help") {
        return cmd_help(&o, o.positional.get(1).map(String::as_str));
    }
    if o.help {
        println!("{}", usage(&load_agents(&o)?));
        return Ok(0);
    }
    match first {
        Some("init") => cmd_init(&o, &o.positional[1..]).map(|()| 0),
        _ => run_agent(o),
    }
}

fn main() {
    match real_main() {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_window_is_the_smallest_known_size() {
        assert_eq!(effective_window(0, 32_768, None), 32_768);
        assert_eq!(effective_window(0, 200_000, Some(128_000)), 128_000);
        assert_eq!(effective_window(64_000, 200_000, Some(128_000)), 64_000);
        assert_eq!(effective_window(0, 0, Some(0)), 0);
    }

    fn p(args: &[&str]) -> Opts {
        parse(args.iter().map(|s| s.to_string()).collect()).unwrap()
    }

    #[test]
    fn parses_agent_task_and_flags() {
        let o = p(&[
            "build",
            "fix",
            "the bug",
            "--workspace",
            "/w",
            "--resume=ab",
        ]);
        assert_eq!(o.positional, ["build", "fix", "the bug"]);
        assert_eq!(
            (o.workspace.as_deref(), o.resume.as_deref()),
            (Some("/w"), Some("ab"))
        );
        let o = p(&[
            "build",
            "--socket=/s.sock",
            "--sessions-dir",
            "/sess",
            "--token-limit=500",
            "--json",
        ]);
        assert_eq!(
            (
                o.socket.as_deref(),
                o.sessions_dir.as_deref(),
                o.token_limit,
                o.json
            ),
            (Some("/s.sock"), Some("/sess"), Some(500), true)
        );
    }

    #[test]
    fn exit_codes() {
        assert_eq!(
            (
                exit_code(Status::Done),
                exit_code(Status::Failed),
                exit_code(Status::Stopped)
            ),
            (0, 1, 2)
        );
    }
}
