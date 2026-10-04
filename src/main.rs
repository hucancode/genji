mod agent;
mod config;
mod llm;
mod socket;
mod storage;
mod tools;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use agent::{Agent, AgentParams};
use config::{AgentDef, Config, dot};
use storage::context::ContextComposer;
use storage::{events, registry};

const COMMANDS: [&str; 5] = ["init", "list", "stop", "instruct", "inspect"];

const USAGE: &str = "usage:
  genji <agent> [task] [--resume ID] [--parent ID] [--workspace DIR] [--provider P] [--label L]
                [--socket PATH] [--sessions-dir DIR] [--token-limit N]
  genji init [agent...] [--force] [--workspace DIR]
  genji list | stop <id...|all> | instruct <id> <text...> | inspect <id>
  genji help [agent|tool] [--json] | --version";

#[derive(Default)]
struct Opts {
    positional: Vec<String>,
    workspace: Option<String>,
    provider: Option<String>,
    label: String,
    resume: Option<String>,
    parent: Option<String>,
    parent_agent: Option<String>,
    instance_id: Option<String>,
    instructions_file: Option<String>,
    socket: Option<String>,
    sessions_dir: Option<String>,
    token_limit: Option<i64>,
    depth: u32,
    json: bool,
    subagent: bool,
    no_control: bool,
    quiet: bool,
    yes: bool,
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
            "--label" => o.label = value()?,
            "--resume" => o.resume = Some(value()?),
            "--parent" => o.parent = Some(value()?),
            "--parent-agent" => o.parent_agent = Some(value()?),
            "--instance-id" => o.instance_id = Some(value()?),
            "--instructions-file" => o.instructions_file = Some(value()?),
            "--socket" => o.socket = Some(value()?),
            "--sessions-dir" => o.sessions_dir = Some(value()?),
            "--token-limit" => {
                o.token_limit = Some(value()?.parse().context("--token-limit needs a number")?)
            }
            "--depth" => o.depth = value()?.parse().context("--depth needs a number")?,
            "--subagent" => o.subagent = true,
            "--no-control" => o.no_control = true,
            "--quiet-startup" => o.quiet = true,
            "--json" => o.json = true,
            "--yes" | "-y" => o.yes = true,
            "--force" => o.force = true,
            "--help" | "-h" => o.help = true,
            "--version" | "-V" => o.version = true,
            f if f.starts_with("--") => bail!("unknown option `{f}`"),
            _ => o.positional.push(arg),
        }
    }
    Ok(o)
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

/// `--sessions-dir`, made absolute so spawned children agree on it.
fn sessions_dir(o: &Opts) -> Result<Option<PathBuf>> {
    o.sessions_dir.as_deref().map(absolute).transpose()
}

/// Session directory for commands that read sessions without loading the config.
fn sessions_for(o: &Opts, ws: &Path) -> Result<PathBuf> {
    Ok(sessions_dir(o)?.unwrap_or_else(|| dot(ws, "sessions")))
}

/// `genji help agent|tool [--json]`: loaded agents or the tool registry.
fn cmd_help(o: &Opts, topic: Option<&str>) -> Result<i32> {
    let agents = config::load_agents(&workspace(o)?);
    match topic {
        Some("agent") if o.json => {
            let arr: Vec<Value> = agents
                .values()
                .map(|a| {
                    json!({ "name": a.name, "description": a.description, "tools": a.tools,
                            "skills": a.skills, "finish": a.finish, "model": a.model, "internal": a.internal })
                })
                .collect();
            println!("{}", serde_json::to_string(&arr)?);
        }
        Some("tool") if o.json => println!("{}", serde_json::to_string(&tools::list())?),
        Some("tool") => {
            for t in tools::list() {
                eprintln!(
                    "  {:<11} {}",
                    t["name"].as_str().unwrap_or_default(),
                    t["description"].as_str().unwrap_or_default()
                );
            }
        }
        None | Some("agent") => print_usage(&agents),
        Some(t) => {
            eprintln!("unknown help topic `{t}` (agent, tool)");
            return Ok(2);
        }
    }
    Ok(0)
}

fn print_usage(agents: &BTreeMap<String, AgentDef>) {
    eprintln!("{USAGE}\n\nagents:");
    if agents.is_empty() {
        eprintln!("  (none; `genji init` writes the default agents to .genji/agents)");
    }
    for a in agents.values().filter(|a| !a.internal) {
        eprintln!("  {:<10} {}", a.name, a.description);
    }
}

fn exit_code(status: &str) -> i32 {
    match status {
        "done" => 0,
        "failed" => 1,
        _ => 2,
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

/// Keeps the control socket and registry entry alive; removes both on drop.
struct Guard {
    instance: registry::Instance,
    control: Arc<socket::Control>,
}

impl Guard {
    /// Point the registry entry at the instance now running, so `genji list` shows it.
    fn rebind(&mut self, id: &str) {
        registry::remove(&self.instance.id);
        self.instance.id = id.to_string();
        if let Err(e) = self.instance.save() {
            eprintln!("[registry] warning: could not register instance: {e:#}");
        }
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.control.shutdown();
        registry::remove(&self.instance.id);
    }
}

fn start_control(
    workspace: &Path,
    socket_path: PathBuf,
    context: Arc<RwLock<ContextComposer>>,
    id: &str,
    label: &str,
    quiet: bool,
) -> Result<Guard> {
    let control = socket::Control::start(socket_path, context)?;
    if !quiet {
        eprintln!("[control] listening on {}", control.path.display());
    }
    let instance = registry::Instance {
        id: id.to_string(),
        pid: std::process::id(),
        workspace: workspace.display().to_string(),
        control_socket: control.path.display().to_string(),
        label: label.to_string(),
        started_at: storage::util::unix_secs(),
    };
    if let Err(e) = instance.save() {
        eprintln!("[registry] warning: could not register instance: {e:#}");
    }
    Ok(Guard { instance, control })
}

fn run_agent(o: Opts) -> Result<i32> {
    let ws = workspace(&o)?;
    let mut cfg = Config::load_or_create(&ws)?;
    cfg.sessions_dir = sessions_dir(&o)?;
    if let Some(n) = o.token_limit {
        cfg.token_limit = n;
    }
    config::ensure_agents(&ws)?;
    let agents = config::load_agents(&ws);
    let sessions = cfg.sessions(&ws);
    let quiet = o.quiet || o.subagent;
    let mut resume = None;
    if let Some(prefix) = &o.resume {
        let s = events::summary(&events::find_session(&sessions, prefix)?)?;
        let id = s["id"].as_str().unwrap_or_default().to_string();
        if registry::find(&id).is_ok() {
            bail!("instance {id} is still running; stop it before resuming");
        }
        resume = Some((id, s["agent"].as_str().unwrap_or_default().to_string()));
    }
    let name = match (o.positional.first(), &resume) {
        (Some(n), _) => n.clone(),
        (None, Some((_, a))) => a.clone(),
        (None, None) => {
            print_usage(&agents);
            return Ok(2);
        }
    };
    let Some(mut def) = agents.get(&name).cloned() else {
        eprintln!("unknown agent or command `{name}`\n");
        print_usage(&agents);
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
    let provider = cfg.provider()?;
    let words = o.positional.get(1..).unwrap_or_default();
    let mut task = read_task(o.instructions_file.as_deref(), words)?;
    let first_id = match (&resume, &o.instance_id) {
        (Some((id, _)), _) => id.clone(),
        (None, Some(id)) => id.clone(),
        (None, None) => registry::new_id(),
    };
    if !quiet {
        eprintln!(
            "[genji] agent={name} provider={} base_url={} instance={first_id}",
            cfg.provider, provider.base_url
        );
    }
    // Top-level runs open a control socket and register, so list/stop/instruct/inspect find them.
    let context = Arc::new(RwLock::new(ContextComposer::new(
        String::new(),
        Vec::new(),
        provider.context_window,
    )));
    let mut guard = if o.no_control || !cfg.control_enabled {
        None
    } else {
        let socket_path = match &o.socket {
            Some(p) => absolute(p)?,
            None => dot(&ws, "control.sock"),
        };
        Some(start_control(
            &ws,
            socket_path,
            context.clone(),
            &first_id,
            &o.label,
            quiet,
        )?)
    };
    if task.is_none() && resume.is_none() {
        let Some(g) = &guard else {
            eprintln!("[genji] no task given and no control socket to wait on");
            return Ok(2);
        };
        g.control.set_status("idle");
        eprintln!(
            "[genji] no task given; waiting on {} (genji instruct {first_id} \"<task>\")",
            g.control.path.display()
        );
        match g.control.wait_for_instruction() {
            Some(queued) => task = Some(queued.join("\n")),
            None => return Ok(0),
        }
    }
    let mut id = first_id;
    let mut parent = o.parent.clone();
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
            control: guard.as_ref().map(|g| g.control.clone()),
            context: context.clone(),
        })?;
        let report = a.run(task.as_deref())?;
        eprintln!("[report] {report}");
        // A `hand_off` continues in this process as a new instance of `next.agent`:
        // new id, new session file, fresh context.
        let next = a
            .verdict
            .as_ref()
            .filter(|_| a.handed_off && a.status == "done")
            .and_then(|v| v.next.clone());
        // An agent that can hand off and ran out of tool iterations continues as a fresh
        // instance of itself; the shared run limits (time, tokens) still bound the chain.
        let next = next.or_else(|| {
            (a.status == "stopped"
                && a.reason == Some("max_iterations")
                && a.def.tools.iter().any(|t| t == "hand_off"))
            .then(|| tools::Next {
                agent: a.def.name.clone(),
                task: {
                    const NOTE: &str = "\n\nA previous instance used up its tool iterations on this task. Inspect the workspace and the task's tracking files for what is already done, then continue with what remains.";
                    let t = task.as_deref().unwrap_or_default();
                    if t.ends_with(NOTE) { t.to_string() } else { format!("{t}{NOTE}") }
                },
            })
        });
        let Some(next) = next else {
            return Ok(exit_code(a.status));
        };
        parent = Some(id.clone());
        id = registry::new_id();
        def = agents[&next.agent].clone();
        task = Some(next.task);
        resuming = false;
        if let Some(g) = &mut guard {
            g.rebind(&id);
        }
        if !quiet {
            eprintln!(
                "[hand_off] {} -> {} (instance {id})",
                a.def.name, def.name
            );
        }
    }
}

fn format_uptime(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m{}s", secs / 60, secs % 60),
        _ => format!("{}h{}m", secs / 3600, (secs % 3600) / 60),
    }
}

/// Running instances with their live status: JSON on stdout, a table on stderr.
fn cmd_list() -> Result<()> {
    let rows = registry::list_live();
    let arr: Vec<Value> = rows
        .iter()
        .map(|(i, status)| {
            json!({ "id": i.id, "pid": i.pid, "uptime_secs": i.uptime_secs(), "workspace": i.workspace,
                    "label": i.label, "control_socket": i.control_socket, "status": status })
        })
        .collect();
    println!("{}", serde_json::to_string(&arr)?);
    if rows.is_empty() {
        eprintln!("no running genji instances");
    }
    for (i, status) in &rows {
        eprintln!(
            "{:<8} {:<7} {:<8} {:<38} {status}",
            i.id,
            i.pid,
            format_uptime(i.uptime_secs()),
            i.workspace
        );
    }
    Ok(())
}

fn cmd_stop(ids: &[String]) -> Result<i32> {
    let targets: Vec<&str> = ids
        .iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if targets.is_empty() {
        eprintln!("`genji stop` needs one or more instance ids, or `all`");
        return Ok(2);
    }
    let mut failed = false;
    let instances: Vec<registry::Instance> = if targets.contains(&"all") {
        registry::list_live().into_iter().map(|(i, _)| i).collect()
    } else {
        targets
            .iter()
            .filter_map(|id| {
                registry::find(id)
                    .inspect_err(|e| {
                        eprintln!("{e:#}");
                        failed = true;
                    })
                    .ok()
            })
            .collect()
    };
    let mut results = Vec::new();
    for i in &instances {
        let r = socket::send(Path::new(&i.control_socket), "/stop");
        failed |= r.is_err();
        let message = r.unwrap_or_else(|e| format!("{e:#}"));
        eprintln!("stop {} (pid {}): {message}", i.id, i.pid);
        results.push(json!({ "id": i.id, "pid": i.pid, "message": message }));
    }
    println!("{}", serde_json::to_string(&results)?);
    Ok(i32::from(failed))
}

fn cmd_instruct(id: &str, text: &str) -> Result<()> {
    if text.trim().is_empty() {
        bail!("missing instruction (usage: genji instruct <id> <text...>)");
    }
    let inst = registry::find(id)?;
    let reply = socket::send(Path::new(&inst.control_socket), text)?;
    // Structured replies (e.g. `/context`) are passed through unchanged.
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&reply) {
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }
    let message = reply.strip_prefix("status:").unwrap_or(&reply).trim();
    println!(
        "{}",
        serde_json::to_string(&json!({ "id": inst.id, "message": message }))?
    );
    eprintln!("{message}");
    Ok(())
}

/// Recorded summary of an instance from its session file, plus live details when it is running.
fn cmd_inspect(id: &str, o: &Opts) -> Result<()> {
    let live = registry::find(id).ok();
    let ws = match &live {
        Some(i) => PathBuf::from(&i.workspace),
        None => workspace(o)?,
    };
    let mut obj = events::summary(&events::find_session(&sessions_for(o, &ws)?, id)?)?;
    if let (Some(map), Some(i)) = (obj.as_object_mut(), &live) {
        map.insert("pid".into(), json!(i.pid));
        map.insert("label".into(), json!(i.label));
        map.insert("control_socket".into(), json!(i.control_socket));
        map.insert("uptime_secs".into(), json!(i.uptime_secs()));
        map.insert("live_status".into(), json!(i.status()));
    }
    println!("{}", serde_json::to_string(&obj)?);
    for key in [
        "id",
        "agent",
        "model",
        "parent",
        "depth",
        "task",
        "status",
        "live_status",
        "tokens_used",
        "messages",
        "pid",
    ] {
        if let Some(v) = obj.get(key).filter(|v| !v.is_null()) {
            eprintln!(
                "{key:<14}{}",
                v.as_str().map_or_else(|| v.to_string(), String::from)
            );
        }
    }
    Ok(())
}

/// `genji init [agent...] [--force]`: writes the default agent files; prints
/// `{"written":[...],"skipped":[...]}`.
fn cmd_init(o: &Opts, only: &[String]) -> Result<()> {
    let r = config::init_agents(&workspace(o)?, o.force, only)?;
    println!("{}", serde_json::to_string(&json!({ "written": r.written, "skipped": r.skipped }))?);
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
        print_usage(&config::load_agents(&workspace(&o)?));
        return Ok(0);
    }
    let rest = o.positional.get(1..).unwrap_or_default();
    match first {
        Some(c) if COMMANDS.contains(&c) => match c {
            "init" => cmd_init(&o, rest).map(|()| 0),
            "list" => cmd_list().map(|()| 0),
            "stop" => cmd_stop(rest),
            "instruct" => match rest {
                [id, text @ ..] => cmd_instruct(id, &text.join(" ")).map(|()| 0),
                [] => bail!("usage: genji instruct <id> <text...>"),
            },
            _ => match rest {
                [id] => cmd_inspect(id, &o).map(|()| 0),
                _ => bail!("usage: genji inspect <id>"),
            },
        },
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
            "-y",
        ]);
        assert_eq!(o.positional, ["build", "fix", "the bug"]);
        assert_eq!(
            (o.workspace.as_deref(), o.resume.as_deref(), o.yes),
            (Some("/w"), Some("ab"), true)
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
            (exit_code("done"), exit_code("failed"), exit_code("stopped")),
            (0, 1, 2)
        );
        assert_eq!(format_uptime(75), "1m15s");
    }
}
