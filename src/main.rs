mod agent;
mod config;
mod llm;
mod socket;
mod storage;
mod tools;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use agent::{Agent, AgentParams};
use config::Config;
use serde_json::json;
use storage::db::Db;
use storage::modes::Mode;

#[derive(Parser, Debug)]
#[command(
    name = "genji",
    version,
    about = "A minimal coding agent",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    task: Option<String>,
    #[arg(long, hide = true, global = true)]
    subagent: bool,
    #[arg(long, hide = true, global = true)]
    parent_instance: Option<String>,
    #[arg(long, hide = true, global = true)]
    instructions_file: Option<String>,
    #[arg(long, default_value = "", global = true)]
    label: String,
    #[arg(long, default_value_t = 0, hide = true, global = true)]
    depth: u32,
    #[arg(long, global = true)]
    quiet_startup: bool,
    #[cfg(feature = "formal")]
    #[arg(long, global = true)]
    formal: bool,
    #[arg(long, global = true)]
    workspace: Option<String>,
    #[arg(long, global = true)]
    provider: Option<String>,
    #[arg(long, hide = true, global = true)]
    no_control: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    Plan {
        task: Option<String>,
    },
    Build {
        task: Option<String>,
    },
    Explore {
        task: Option<String>,
    },
    Retro {
        task: Option<String>,
    },
    List,
    Stop {
        ids: Vec<String>,
    },
    Instruct {
        /// Instance id (see `genji list`).
        id: String,
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        instruction: Vec<String>,
    },
    Setplan {
        /// Instance id (see `genji list`).
        id: String,
        /// Plan slug (the `<slug>.md` file under the plans directory).
        slug: String,
    },
    Context {
        /// Instance id (see `genji list`).
        id: String,
    },
    Inspect {
        /// Instance id (see `genji list`).
        id: String,
    },
    Reset {
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

fn ensure_layout(cfg: &Config, workspace: &std::path::Path, formal: bool) -> Result<()> {
    #[cfg(feature = "formal")]
    let mut dirs = vec![cfg.plans_path(workspace), cfg.skills_path(workspace)];
    #[cfg(not(feature = "formal"))]
    let dirs = vec![cfg.plans_path(workspace), cfg.skills_path(workspace)];
    #[cfg(feature = "formal")]
    if formal {
        dirs.push(cfg.requirements_path(workspace));
        dirs.push(cfg.tickets_path(workspace));
    }
    #[cfg(not(feature = "formal"))]
    let _ = formal;
    for d in dirs {
        std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    }
    Ok(())
}

const DEFAULT_TASK: &str = "Satisfy the active requirements in .genji/requirements/. Derive system requirements and tickets as needed.";

fn read_task(instructions_file: Option<&str>, task: Option<&str>) -> Result<Option<String>> {
    if let Some(f) = instructions_file {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading instructions file {f}"))?;
        if !text.trim().is_empty() {
            return Ok(Some(text));
        }
    }
    if let Some(t) = task
        && !t.trim().is_empty()
    {
        return Ok(Some(t.to_string()));
    }
    Ok(None)
}

#[derive(Debug, PartialEq, Eq)]
enum Startup {
    Run(String),
    Wait,
}

fn startup_action(
    explicit_task: Option<String>,
    active_requirements: i64,
    formal: bool,
) -> Startup {
    if let Some(t) = explicit_task {
        return Startup::Run(t);
    }
    if formal && active_requirements > 0 {
        return Startup::Run(DEFAULT_TASK.to_string());
    }
    Startup::Wait
}

fn wait_for_instruction(
    control: &socket::Control,
    id: &str,
    quiet: bool,
) -> Result<Option<String>> {
    control.set_status("idle");
    if !quiet {
        eprintln!(
            "[genji] no instruction provided; waiting for one on {}",
            control.path.display()
        );
        eprintln!("[genji] send one with: genji instruct {id} \"<instruction>\"");
    }
    loop {
        if control.stop_requested() {
            if !quiet {
                eprintln!("[genji] stop requested before any instruction; exiting");
            }
            return Ok(None);
        }
        let queued = control.drain();
        if !queued.is_empty() {
            return Ok(Some(queued.join("\n")));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[cfg(feature = "formal")]
fn mode_switch_instruction(mode: Mode) -> String {
    match mode {
        Mode::Plan => "Now run in PLAN mode: assess progress against the requirements, update requirements/tickets, and stop when the plan is current.".into(),
        Mode::Build => "Now run in BUILD mode: work the open tickets, verify your changes, and resolve tickets when done.".into(),
        Mode::Explore => "Now run in EXPLORE mode: investigate and report findings.".into(),
        Mode::Retro => "Now run in RETRO mode: analyze history and improve prompts/skills.".into(),
    }
}

struct RunRequest {
    cfg: Config,
    workspace: PathBuf,
    db: Db,
    instance_id: String,
    mode: Mode,
    task: String,
    formal: bool,
    quiet: bool,
    control: Option<Arc<socket::Control>>,
    context: Arc<RwLock<storage::context::ContextComposer>>,
}

fn run_single(req: RunRequest, parent: Option<String>, depth: u32) -> Result<String> {
    let RunRequest {
        cfg,
        workspace,
        db,
        instance_id,
        mode,
        task,
        formal,
        quiet,
        control,
        context,
    } = req;
    if !quiet {
        let model = cfg.model_for_mode(mode);
        eprintln!(
            "[genji] mode={} model={} instance={} task={}",
            mode.as_str(),
            model,
            instance_id,
            llm::truncate(task.clone(), 120)
        );
    }
    let mut agent = Agent::new(AgentParams {
        cfg,
        workspace,
        db,
        instance_id,
        parent_instance: parent,
        mode,
        depth,
        task: task.clone(),
        formal,
        control,
        context,
    })?;
    agent.add_user(&task)?;
    let report = agent.run_loop()?;
    let status = agent.status();
    agent.finish(status, &report)?;
    Ok(report)
}

#[cfg(feature = "formal")]
fn run_cycle(req: RunRequest) -> Result<String> {
    let RunRequest {
        cfg,
        workspace,
        db,
        instance_id,
        mode: start_mode,
        task,
        formal,
        quiet,
        control,
        context,
    } = req;
    if !quiet {
        eprintln!(
            "[genji] auto-cycle instance={} start={} max_cycles={}",
            instance_id,
            start_mode.as_str(),
            cfg.max_cycles
        );
    }
    let max_cycles = cfg.max_cycles;
    let mut agent = Agent::new(AgentParams {
        cfg,
        workspace,
        db,
        instance_id,
        parent_instance: None,
        mode: start_mode,
        depth: 0,
        task: task.clone(),
        formal,
        control,
        context,
    })?;
    agent.add_user(&task)?;

    let mut current = start_mode;
    let mut last_report = String::new();
    for cycle in 0..max_cycles {
        let active = storage::reqmd::active_count(&agent.cfg, &agent.workspace)?;
        agent
            .events
            .cycle(cycle + 1, max_cycles, current.as_str(), active);
        if cycle > 0 && active == 0 {
            last_report =
                format!("All requirements are met (0 active). Stopped after {cycle} cycle(s).");
            if !quiet {
                eprintln!("[cycle] {last_report}");
            }
            break;
        }
        if !quiet {
            eprintln!(
                "[cycle {}/{}] mode={} active_requirements={} tokens={}",
                cycle + 1,
                max_cycles,
                current.as_str(),
                active,
                agent.tokens_used
            );
        }
        agent.set_mode(current)?;
        if cycle > 0 {
            agent.add_user(&mode_switch_instruction(current))?;
        }
        let report = agent.run_loop()?;
        let stopped = agent
            .control
            .as_ref()
            .map(|c| c.stop_requested())
            .unwrap_or(false);
        if !quiet {
            eprintln!(
                "[cycle {}] {} done: {}",
                cycle + 1,
                current.as_str(),
                llm::truncate(report.clone(), 300)
            );
        }
        last_report = report;
        if stopped {
            eprintln!("[cycle] stop requested; ending cycle");
            break;
        }
        if agent.failed {
            eprintln!("[cycle] LLM failure; ending cycle");
            break;
        }
        current = match current {
            Mode::Plan => Mode::Build,
            Mode::Build => Mode::Plan,
            _ => Mode::Plan,
        };
    }
    let status = agent.status();
    agent.finish(status, &last_report)?;
    Ok(last_report)
}

struct InstanceGuard(storage::registry::Instance);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        storage::registry::remove(&self.0.id);
    }
}

fn format_uptime(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{}s", secs / 60, secs % 60)
    } else {
        format!("{}h{}m", secs / 3600, (secs % 3600) / 60)
    }
}

/// Strip the `status:` prefix the control protocol uses and trim whitespace.
fn status_text(reply: &str) -> String {
    reply
        .strip_prefix("status:")
        .unwrap_or(reply)
        .trim()
        .to_string()
}

fn query_status(socket: &str) -> String {
    match socket::send(Path::new(socket), "/status") {
        Ok(r) => status_text(&r),
        Err(e) => format!("(unreachable: {e:#})"),
    }
}

fn is_root_instance(_inst: &storage::registry::Instance) -> bool {
    // TODO: fix this
    true
}

/// `genji list` — running instances with their live status. stdout is machine
/// output (JSON); the human table is written to stderr.
fn cmd_list() -> Result<()> {
    let rows: Vec<(storage::registry::Instance, String)> = storage::registry::list_live()
        .into_iter()
        .map(|inst| {
            let status = query_status(&inst.control_socket);
            (inst, status)
        })
        .collect();
    let arr: Vec<serde_json::Value> = rows
        .iter()
        .map(|(inst, status)| {
            json!({
                "id": inst.id,
                "root": is_root_instance(inst),
                "pid": inst.pid,
                "uptime_secs": inst.uptime_secs(),
                "workspace": inst.workspace,
                "label": inst.label,
                "control_socket": inst.control_socket,
                "status": status,
            })
        })
        .collect();
    // Terse machine output: just the instances, no `type`/wrapper boilerplate.
    println!("{}", serde_json::to_string(&arr)?);
    if rows.is_empty() {
        eprintln!("no running genji instances");
        return Ok(());
    }
    eprintln!(
        "{:<8} {:<4} {:<7} {:<8} {:<38} STATUS",
        "ID", "ROOT", "PID", "UPTIME", "WORKSPACE"
    );
    for (inst, status) in &rows {
        eprintln!(
            "{:<8} {:<4} {:<7} {:<8} {:<38} {}",
            inst.id,
            if is_root_instance(inst) { "*" } else { "" },
            inst.pid,
            format_uptime(inst.uptime_secs()),
            inst.workspace,
            status
        );
    }
    Ok(())
}

fn cmd_stop(ids: &[String]) -> Result<()> {
    let targets: Vec<String> = ids
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if targets.is_empty() {
        eprintln!("warning: `genji stop` needs one or more instance ids, or `all`");
        let instances = storage::registry::list_live();
        if instances.is_empty() {
            eprintln!("no running genji instances");
        } else {
            eprintln!("running instances:");
            for inst in &instances {
                eprintln!(
                    "  {}  pid={}  {}{}",
                    inst.id,
                    inst.pid,
                    inst.workspace,
                    if inst.label.is_empty() {
                        String::new()
                    } else {
                        format!("  ({})", inst.label)
                    }
                );
            }
            eprintln!("use `genji stop all` or `genji stop <id>...`");
        }
        std::process::exit(2);
    }

    let mut failed = false;
    let instances = if targets.iter().any(|t| t == "all") {
        storage::registry::list_live()
    } else {
        let mut found = Vec::new();
        for id in &targets {
            match storage::registry::find(id) {
                Ok(inst) => found.push(inst),
                Err(e) => {
                    eprintln!("{e:#}");
                    failed = true;
                }
            }
        }
        found
    };

    if instances.is_empty() && !failed {
        eprintln!("no running genji instances");
    }
    let mut results: Vec<serde_json::Value> = Vec::new();
    for inst in &instances {
        match socket::send(Path::new(&inst.control_socket), "/stop") {
            Ok(r) => {
                eprintln!("stopping {} (pid {}): {}", inst.id, inst.pid, r);
                results.push(json!({
                    "id": inst.id,
                    "pid": inst.pid,
                    "ok": true,
                    "message": r,
                }));
            }
            Err(e) => {
                eprintln!("failed to stop {}: {e:#}", inst.id);
                results.push(json!({
                    "id": inst.id,
                    "pid": inst.pid,
                    "ok": false,
                    "message": format!("{e:#}"),
                }));
                failed = true;
            }
        }
    }
    println!("{}", serde_json::to_string(&results)?);
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn db_files(db_path: &Path) -> [PathBuf; 3] {
    let base = db_path.as_os_str().to_string_lossy();
    [
        db_path.to_path_buf(),
        PathBuf::from(format!("{base}-wal")),
        PathBuf::from(format!("{base}-shm")),
    ]
}

fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => collect_files(&path, out),
            _ => out.push(path),
        }
    }
}

/// `genji reset` — wipe the workspace database, plans, requirements, and open
/// tickets, then recreate an empty database.
///
/// The whole `.genji/plans`, `.genji/requirements`, and `.genji/tickets` trees
/// are removed along with the SQLite database (and its WAL/SHM sidecars); the
/// config and skills are left untouched. The user is told how many files will
/// be destroyed before anything is deleted.
fn cmd_reset(workspace: &Path, assume_yes: bool) -> Result<()> {
    let cfg = Config::load_or_create(workspace)?;
    let db_path = cfg.db_file(workspace);
    let plans_dir = cfg.plans_path(workspace);
    #[cfg(feature = "formal")]
    let requirements_dir = cfg.requirements_path(workspace);
    #[cfg(feature = "formal")]
    let tickets_dir = cfg.tickets_path(workspace);
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let here = canon(workspace);
    for inst in storage::registry::list_live() {
        if canon(Path::new(&inst.workspace)) == here {
            bail!(
                "instance {} (pid {}) is running in this workspace; stop it first with `genji stop {}`",
                inst.id,
                inst.pid,
                inst.id
            );
        }
    }
    let db_existing: Vec<PathBuf> = db_files(&db_path)
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    #[cfg(feature = "formal")]
    let mut requirement_files = Vec::new();
    #[cfg(feature = "formal")]
    collect_files(&requirements_dir, &mut requirement_files);
    let mut plan_files = Vec::new();
    collect_files(&plans_dir, &mut plan_files);
    #[cfg(feature = "formal")]
    let mut ticket_files = Vec::new();
    #[cfg(feature = "formal")]
    collect_files(&tickets_dir, &mut ticket_files);
    let total = db_existing.len() + plan_files.len() + {
        #[cfg(feature = "formal")]
        {
            requirement_files.len() + ticket_files.len()
        }
        #[cfg(not(feature = "formal"))]
        {
            0
        }
    };

    if total == 0 {
        eprintln!(
            "[reset] nothing to delete; database, plans, requirements and tickets are already empty"
        );
    } else {
        #[cfg(feature = "formal")]
        eprintln!(
            "[reset] this will delete {total} file(s): {} database file(s), {} plan(s) in {}, {} requirement(s) in {}, {} open ticket(s) in {}",
            db_existing.len(),
            plan_files.len(),
            plans_dir.display(),
            requirement_files.len(),
            requirements_dir.display(),
            ticket_files.len(),
            tickets_dir.display(),
        );
        #[cfg(not(feature = "formal"))]
        eprintln!(
            "[reset] this will delete {total} file(s): {} database file(s), {} plan(s) in {}",
            db_existing.len(),
            plan_files.len(),
            plans_dir.display(),
        );
        if !assume_yes {
            if !std::io::stdin().is_terminal() {
                bail!("refusing to delete {total} file(s) without confirmation; re-run with --yes");
            }
            eprint!("Delete {total} file(s) and start over? [y/N] ");
            use std::io::Write;
            std::io::stderr().flush().ok();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                eprintln!("[reset] aborted");
                return Ok(());
            }
        }
        for p in &db_existing {
            std::fs::remove_file(p).with_context(|| format!("deleting {}", p.display()))?;
        }
        #[cfg(feature = "formal")]
        if requirements_dir.exists() {
            std::fs::remove_dir_all(&requirements_dir)
                .with_context(|| format!("deleting {}", requirements_dir.display()))?;
        }
        if plans_dir.exists() {
            std::fs::remove_dir_all(&plans_dir)
                .with_context(|| format!("deleting {}", plans_dir.display()))?;
        }
        #[cfg(feature = "formal")]
        if tickets_dir.exists() {
            std::fs::remove_dir_all(&tickets_dir)
                .with_context(|| format!("deleting {}", tickets_dir.display()))?;
        }
        eprintln!("[reset] deleted {total} file(s)");
    }
    std::fs::create_dir_all(&plans_dir)
        .with_context(|| format!("creating {}", plans_dir.display()))?;
    #[cfg(feature = "formal")]
    for d in [&requirements_dir, &tickets_dir] {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    let db = Db::open(&db_path)?;
    db.init_schema()?;
    storage::prompts::seed_prompts(&db)?;
    eprintln!(
        "[reset] initialized clean database at {}",
        db_path.display()
    );
    #[cfg(feature = "formal")]
    let deleted: Vec<String> = db_existing
        .iter()
        .chain(plan_files.iter())
        .chain(requirement_files.iter())
        .chain(ticket_files.iter())
        .map(|p| p.display().to_string())
        .collect();
    #[cfg(not(feature = "formal"))]
    let deleted: Vec<String> = db_existing
        .iter()
        .chain(plan_files.iter())
        .map(|p| p.display().to_string())
        .collect();
    println!(
        "{}",
        serde_json::to_string(&json!({
            "workspace": workspace.display().to_string(),
            "db": db_path.display().to_string(),
            "deleted": deleted,
        }))?
    );
    Ok(())
}

fn cmd_instruct(id: &str, instruction: &str) -> Result<()> {
    if instruction.trim().is_empty() {
        bail!("missing instruction (usage: genji instruct <id> <instruction>)");
    }
    let inst = storage::registry::find(id)?;
    let resp = socket::send(Path::new(&inst.control_socket), instruction)?;
    let message = status_text(&resp);
    println!(
        "{}",
        serde_json::to_string(&json!({ "id": inst.id, "message": message }))?
    );
    eprintln!("{message}");
    Ok(())
}

fn cmd_setplan(id: &str, slug: &str) -> Result<()> {
    if slug.trim().is_empty() {
        bail!("missing plan slug (usage: genji setplan <id> <slug>)");
    }
    let inst = storage::registry::find(id)?;
    let resp = socket::send(Path::new(&inst.control_socket), &format!("/setplan {slug}"))?;
    let message = status_text(&resp);
    println!(
        "{}",
        serde_json::to_string(&json!({ "id": inst.id, "plan": slug, "message": message }))?
    );
    eprintln!("{message}");
    Ok(())
}

fn cmd_context(id: &str) -> Result<()> {
    let inst = storage::registry::find(id)?;
    let resp = socket::send(Path::new(&inst.control_socket), "/context")?;
    let value: serde_json::Value = serde_json::from_str(&resp)
        .with_context(|| format!("unexpected /context reply: {resp}"))?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn find_trace(instance: &str) -> Result<std::path::PathBuf> {
    let dir = storage::registry::events_dir();
    let exact = storage::registry::events_path(instance);
    if exact.exists() {
        return Ok(exact);
    }
    let mut matches: Vec<std::path::PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if stem.starts_with(instance) {
                matches.push(p);
            }
        }
    }
    match matches.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no event trace for instance `{instance}`"),
        many => {
            let names: Vec<&str> = many
                .iter()
                .filter_map(|p| p.file_stem().and_then(|s| s.to_str()))
                .collect();
            bail!(
                "instance id `{instance}` is ambiguous; matches: {} (use a longer prefix)",
                names.join(", ")
            )
        }
    }
}

struct TraceTarget {
    instance_id: String,
    trace_path: std::path::PathBuf,
    instance: Option<storage::registry::Instance>,
}

fn resolve_target(id: &str) -> Result<TraceTarget> {
    let id = id.trim();
    if id.is_empty() {
        bail!("missing instance id");
    }
    if let Ok(inst) = storage::registry::find(id) {
        let trace_path = storage::registry::events_path(&inst.id);
        return Ok(TraceTarget {
            instance_id: inst.id.clone(),
            trace_path,
            instance: Some(inst),
        });
    }
    Ok(TraceTarget {
        instance_id: id.to_string(),
        trace_path: find_trace(id)?,
        instance: None,
    })
}

fn read_trace(target: &TraceTarget) -> Result<Option<String>> {
    if target.trace_path.exists() {
        Ok(Some(
            std::fs::read_to_string(&target.trace_path)
                .with_context(|| format!("reading event trace {}", target.trace_path.display()))?,
        ))
    } else if target.instance.is_some() {
        Ok(None)
    } else {
        bail!("no event trace for instance `{}`", target.instance_id)
    }
}

fn trace_instance_start(text: &str) -> Option<serde_json::Value> {
    text.lines().find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        (v.get("type").and_then(|t| t.as_str()) == Some("instance_start")).then_some(v)
    })
}

fn is_instance_end(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(|t| t == "instance_end")
        })
        .unwrap_or(false)
}

fn query_context(socket: &str) -> Option<storage::context::ContextInfo> {
    let reply = socket::send(Path::new(socket), "/context stats").ok()?;
    let value: serde_json::Value = serde_json::from_str(&reply).ok()?;
    Some(storage::context::ContextInfo::from_json(&value))
}

fn cmd_inspect(id: &str) -> Result<()> {
    let target = resolve_target(id)?;
    let text = read_trace(&target)?.unwrap_or_default();
    let count = text.lines().filter(|l| !l.trim().is_empty()).count();
    let ended = text.lines().any(is_instance_end);
    let start = trace_instance_start(&text);
    let status = target
        .instance
        .as_ref()
        .map(|i| query_status(&i.control_socket));
    let context = target
        .instance
        .as_ref()
        .and_then(|i| query_context(&i.control_socket));

    let mut obj = json!({
        "type": "instance",
        "id": id.trim(),
        "trace": target.trace_path.display().to_string(),
        "events": count,
        "ended": ended,
    });
    let map = obj.as_object_mut().expect("object");
    if let Some(info) = &context {
        map.insert("context".into(), info.to_json());
    }
    if let Some(s) = &start {
        for key in ["workspace", "mode", "model", "parent", "depth", "task"] {
            if let Some(v) = s.get(key) {
                map.insert(key.to_string(), v.clone());
            }
        }
    }
    if let Some(inst) = &target.instance {
        map.insert("pid".into(), json!(inst.pid));
        map.insert("label".into(), json!(inst.label));
        map.entry("workspace".to_string())
            .or_insert_with(|| json!(inst.workspace));
        map.insert("control_socket".into(), json!(inst.control_socket));
        map.insert("uptime_secs".into(), json!(inst.uptime_secs()));
        if let Some(st) = &status {
            map.insert("status".into(), json!(st));
        }
    }
    println!("{}", serde_json::to_string(&obj)?);
    eprintln!("id:      {}", id.trim());
    eprintln!("trace:   {}", target.trace_path.display());
    eprintln!("events:  {count}{}", if ended { " (ended)" } else { "" });
    if let Some(s) = &start {
        let mode = s.get("mode").and_then(|v| v.as_str()).unwrap_or("?");
        let model = s.get("model").and_then(|v| v.as_str()).unwrap_or("?");
        let task = s.get("task").and_then(|v| v.as_str()).unwrap_or("");
        eprintln!("mode:    {mode}  model: {model}");
        if let Some(parent) = s.get("parent").and_then(|v| v.as_str()) {
            eprintln!("parent:  {parent}");
        }
        if !task.is_empty() {
            eprintln!("task:    {task}");
        }
    }
    if let Some(inst) = &target.instance {
        eprintln!(
            "instance: {}  pid: {}  workspace: {}",
            inst.id, inst.pid, inst.workspace
        );
    }
    if let Some(st) = &status {
        eprintln!("status:  {st}");
    }
    if let Some(info) = &context {
        for line in info.summary().lines() {
            eprintln!("{line}");
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        Some(Command::List) => return cmd_list(),
        Some(Command::Stop { ids }) => return cmd_stop(ids),
        Some(Command::Instruct { id, instruction }) => {
            let text = instruction.join(" ");
            return cmd_instruct(id, &text);
        }
        Some(Command::Setplan { id, slug }) => return cmd_setplan(id, slug),
        Some(Command::Context { id }) => return cmd_context(id),
        Some(Command::Inspect { id }) => return cmd_inspect(id),
        Some(Command::Reset { yes }) => {
            let workspace = match &cli.workspace {
                Some(w) => PathBuf::from(w),
                None => std::env::current_dir().context("resolving current directory")?,
            };
            return cmd_reset(&workspace, *yes);
        }
        _ => {}
    }
    let workspace = match &cli.workspace {
        Some(w) => PathBuf::from(w),
        None => std::env::current_dir().context("resolving current directory")?,
    };
    let mut cfg = Config::load_or_create(&workspace)?;
    // Provider precedence: --provider > GENJI_PROVIDER > config.provider.
    if let Some(p) = &cli.provider {
        cfg.provider = p.clone();
    } else if let Ok(p) = std::env::var("GENJI_PROVIDER")
        && !p.trim().is_empty()
    {
        cfg.provider = p;
    }
    #[cfg(feature = "formal")]
    let formal = cli.formal;
    #[cfg(not(feature = "formal"))]
    let formal = false;
    ensure_layout(&cfg, &workspace, formal)?;

    let db = Db::open(&cfg.db_file(&workspace))?;
    db.init_schema()?;

    let synced = tools::skills::sync_skills(&db, &cfg.skills_path(&workspace)).unwrap_or(0);
    #[cfg(feature = "formal")]
    if formal && cfg.auto_ingest_requirements {
        let total = storage::reqmd::sync(&cfg, &workspace)?;
        if total > 0 && !cli.quiet_startup {
            eprintln!(
                "[requirements] loaded {total} md file(s) from {}",
                cfg.requirements_path(&workspace).display()
            );
        }
    }
    #[cfg(feature = "formal")]
    if formal {
        storage::ticketmd::sync(&cfg, &workspace)?;
    }
    storage::prompts::seed_prompts(&db)?;
    if synced > 0 && !cli.quiet_startup {
        eprintln!("[skills] synced {synced} skill file(s)");
    }
    let (start_mode, task_arg): (Mode, Option<&str>) = match &cli.command {
        Some(Command::Plan { task }) => (Mode::Plan, task.as_deref()),
        Some(Command::Build { task }) => (Mode::Build, task.as_deref()),
        Some(Command::Explore { task }) => (Mode::Explore, task.as_deref()),
        Some(Command::Retro { task }) => (Mode::Retro, task.as_deref()),
        Some(_) => unreachable!("instance subcommand handled above"),
        None => (Mode::Build, cli.task.as_deref()),
    };
    let explicit_task = read_task(cli.instructions_file.as_deref(), task_arg)?;
    let quiet = cli.quiet_startup || cli.subagent;
    let instance_id = storage::registry::new_id();
    {
        let trace = storage::registry::events_path(&instance_id);
        if let Some(parent) = trace.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&trace);
    }

    if !quiet {
        let p = cfg.resolve_active_provider();
        eprintln!(
            "[genji] provider={} kind={} base_url={} model={}",
            cfg.provider,
            p.kind,
            p.base_url,
            cfg.model_for_mode(start_mode)
        );
    }
    let context = agent::build_context(&cfg, &db, start_mode, formal)?;

    // Top-level runs open a control socket so instructions can be injected
    // mid-run; subagents never do. Controllable runs also register themselves so
    // `genji list`/`stop`/`instruct`/`inspect` can find them from anywhere.
    let mut _instance_guard: Option<InstanceGuard> = None;
    let control = if cli.no_control || !cfg.control_enabled {
        None
    } else {
        let c = socket::Control::start(
            cfg.control_path(&workspace),
            cfg.plans_path(&workspace),
            context.clone(),
        )?;
        if !quiet {
            eprintln!("[control] listening on {}", c.path.display());
        }
        let inst = storage::registry::Instance {
            id: instance_id.clone(),
            pid: std::process::id(),
            workspace: workspace.display().to_string(),
            control_socket: c.path.display().to_string(),
            label: cli.label.clone(),
            started_at: storage::registry::now_secs(),
        };
        if let Err(e) = inst.save() {
            eprintln!("[registry] warning: could not register instance: {e:#}");
        }
        _instance_guard = Some(InstanceGuard(inst));
        Some(c)
    };
    #[cfg(feature = "formal")]
    let active_requirements = if formal {
        storage::reqmd::active_count(&cfg, &workspace)?
    } else {
        0
    };
    #[cfg(not(feature = "formal"))]
    let active_requirements = 0;
    let task = match startup_action(explicit_task, active_requirements, formal) {
        Startup::Run(t) => t,
        Startup::Wait => match &control {
            Some(c) => match wait_for_instruction(c, &instance_id, quiet)? {
                Some(t) => t,
                None => {
                    c.shutdown();
                    return Ok(());
                }
            },
            None => {
                if !quiet {
                    eprintln!(
                        "[genji] no instruction and no active requirements; \
                         no control socket to wait on. Nothing to do."
                    );
                }
                return Ok(());
            }
        },
    };

    let request = RunRequest {
        cfg,
        workspace,
        db,
        instance_id,
        mode: start_mode,
        task,
        formal,
        quiet,
        control: control.clone(),
        context,
    };
    #[cfg(feature = "formal")]
    let report = if formal {
        run_cycle(request)?
    } else {
        let parent = cli.parent_instance.clone();
        run_single(request, parent, cli.depth)?
    };
    #[cfg(not(feature = "formal"))]
    let report = {
        let parent = cli.parent_instance.clone();
        run_single(request, parent, cli.depth)?
    };

    if let Some(c) = &control {
        c.shutdown();
    }
    eprintln!("[report] {report}");
    Ok(())
}
