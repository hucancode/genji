mod agent;
mod config;
mod llm;
mod socket;
mod storage;
mod tools;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use agent::{Agent, AgentParams};
use config::{AgentDef, Config, dot};
use storage::context::ContextComposer;
use storage::{events, registry};

const COMMANDS: [&str; 5] = ["list", "stop", "instruct", "inspect", "reset"];
const DEFAULT_FOLLOW: u32 = 10;

const USAGE: &str = "usage:
  genji <agent> [task] [--follow[=N]] [--resume ID] [--parent ID] [--workspace DIR] [--provider P] [--label L]
  genji list | stop <id...|all> | instruct <id> <text...> | inspect <id> | reset [-y]
  genji --help | --version";

#[derive(Default)]
struct Opts {
    positional: Vec<String>,
    workspace: Option<String>,
    provider: Option<String>,
    label: String,
    follow: Option<u32>,
    resume: Option<String>,
    parent: Option<String>,
    parent_agent: Option<String>,
    instance_id: Option<String>,
    instructions_file: Option<String>,
    depth: u32,
    subagent: bool,
    no_control: bool,
    quiet: bool,
    yes: bool,
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
            "--depth" => o.depth = value()?.parse().context("--depth needs a number")?,
            "--follow" => {
                o.follow = Some(match &inline {
                    Some(n) => n.parse().context("--follow=N needs a number")?,
                    None => DEFAULT_FOLLOW,
                })
            }
            "--subagent" => o.subagent = true,
            "--no-control" => o.no_control = true,
            "--quiet-startup" => o.quiet = true,
            "--yes" | "-y" => o.yes = true,
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

fn print_usage(agents: &BTreeMap<String, AgentDef>) {
    eprintln!("{USAGE}\n\nagents:");
    for a in agents.values() {
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
    context: Arc<RwLock<ContextComposer>>,
    id: &str,
    label: &str,
    quiet: bool,
) -> Result<Guard> {
    let control = socket::Control::start(dot(workspace, "control.sock"), context)?;
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
    let agents = config::load_agents(&ws);
    let sessions = dot(&ws, "sessions");
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
        Some(start_control(
            &ws,
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
    let mut hops = 0u32;
    loop {
        let follow = if o.subagent { None } else { o.follow };
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
        a.follow_capped = follow.is_some_and(|n| hops >= n);
        let report = a.run(task.as_deref())?;
        eprintln!("[report] {report}");
        let next = a
            .verdict
            .as_ref()
            .filter(|_| a.status == "done")
            .and_then(|v| v.next.clone());
        let Some(next) = next.filter(|_| follow.is_some_and(|n| hops < n)) else {
            return Ok(exit_code(a.status));
        };
        // The handoff continues in this process as a new instance: new id, new session file, fresh context.
        hops += 1;
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
                "[follow] handoff {hops}: {} -> {} (instance {id})",
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
    let mut obj = events::summary(&events::find_session(&dot(&ws, "sessions"), id)?)?;
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

fn count_files(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| {
            if e.path().is_dir() {
                count_files(&e.path())
            } else {
                1
            }
        })
        .sum()
}

/// Delete the workspace's sessions, plans and claims. Agents, skills and config stay.
fn cmd_reset(o: &Opts) -> Result<()> {
    let ws = workspace(o)?;
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if let Some((i, _)) = registry::list_live()
        .into_iter()
        .find(|(i, _)| canon(Path::new(&i.workspace)) == canon(&ws))
    {
        bail!(
            "instance {} (pid {}) is running in this workspace; stop it first with `genji stop {}`",
            i.id,
            i.pid,
            i.id
        );
    }
    let dirs: Vec<PathBuf> = ["sessions", "plans", "claims"]
        .iter()
        .map(|d| dot(&ws, d))
        .collect();
    let total: usize = dirs.iter().map(|d| count_files(d)).sum();
    if total == 0 {
        eprintln!("[reset] nothing to delete");
        return Ok(());
    }
    eprintln!(
        "[reset] this will delete {total} file(s) under {}",
        dot(&ws, "").display()
    );
    if !o.yes {
        if !std::io::stdin().is_terminal() {
            bail!("refusing to delete {total} file(s) without confirmation; re-run with --yes");
        }
        eprint!("Delete {total} file(s) and start over? [y/N] ");
        std::io::stderr().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            eprintln!("[reset] aborted");
            return Ok(());
        }
    }
    for d in dirs.iter().filter(|d| d.exists()) {
        std::fs::remove_dir_all(d).with_context(|| format!("deleting {}", d.display()))?;
    }
    println!(
        "{}",
        serde_json::to_string(&json!({ "workspace": ws.display().to_string(), "deleted": total }))?
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
    if o.help || first == Some("help") {
        print_usage(&config::load_agents(&workspace(&o)?));
        return Ok(0);
    }
    let rest = o.positional.get(1..).unwrap_or_default();
    match first {
        Some(c) if COMMANDS.contains(&c) => match c {
            "list" => cmd_list().map(|()| 0),
            "stop" => cmd_stop(rest),
            "instruct" => match rest {
                [id, text @ ..] => cmd_instruct(id, &text.join(" ")).map(|()| 0),
                [] => bail!("usage: genji instruct <id> <text...>"),
            },
            "inspect" => match rest {
                [id] => cmd_inspect(id, &o).map(|()| 0),
                _ => bail!("usage: genji inspect <id>"),
            },
            _ => cmd_reset(&o).map(|()| 0),
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
            "--follow=3",
            "--workspace",
            "/w",
            "--resume=ab",
            "-y",
        ]);
        assert_eq!(o.positional, ["build", "fix", "the bug"]);
        assert_eq!(
            (o.follow, o.workspace.as_deref(), o.resume.as_deref(), o.yes),
            (Some(3), Some("/w"), Some("ab"), true)
        );
        assert_eq!(p(&["plan", "--follow"]).follow, Some(DEFAULT_FOLLOW));
        assert_eq!(p(&["plan"]).follow, None);
    }

    #[test]
    fn rejects_bad_flags() {
        let args = |a: &[&str]| parse(a.iter().map(|s| s.to_string()).collect());
        assert!(args(&["build", "--nope"]).is_err());
        assert!(args(&["build", "--workspace"]).is_err());
        assert!(args(&["build", "--follow=x"]).is_err());
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
