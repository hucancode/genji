mod agent;
mod config;
mod control;
mod db;
mod events;
mod llm;
mod modes;
mod proc;
mod prompts;
mod registry;
mod reqmd;
mod tools;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent::Agent;
use config::Config;
use db::Db;
use events::EventEmitter;
use modes::Mode;
use serde_json::json;

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
    #[arg(long, global = true)]
    cycle: bool,
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
    #[arg(long, global = true)]
    verbose: bool,
    #[arg(long, global = true)]
    interactive: bool,
    #[arg(long, global = true)]
    workspace: Option<String>,
    #[arg(long, global = true)]
    provider: Option<String>,
    #[arg(long, global = true)]
    init: bool,
    #[arg(long, hide = true, global = true)]
    no_control: bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Convert stakeholder intent into system requirements and tickets.
    Plan {
        /// The user request / task.
        task: Option<String>,
    },
    /// Implement open tickets, verify, and resolve them.
    Build {
        /// The user request / task.
        task: Option<String>,
    },
    /// Investigate and report; no ticket/requirement tools.
    Explore {
        /// The user request / task.
        task: Option<String>,
    },
    /// Study recorded history and improve prompts/skills.
    Retro {
        /// The user request / task.
        task: Option<String>,
    },
    /// List running genji instances and their brief status.
    List,
    /// Stop running genji instances (`all` stops every instance).
    Stop {
        /// Instance ids (space- or comma-separated). Run with no ids for help.
        ids: Vec<String>,
    },
    /// Send an instruction to a running genji instance.
    Instruct {
        /// Instance id (see `genji list`).
        id: String,
        /// Instruction text to deliver.
        #[arg(required = true, num_args = 1.., trailing_var_arg = true, allow_hyphen_values = true)]
        instruction: Vec<String>,
    },
    Inspect {
        /// Instance id (see `genji list`).
        id: String,
    },
    /// Delete the workspace database and start over with a clean one.
    Reset {
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

fn ensure_layout(cfg: &Config, workspace: &std::path::Path) -> Result<()> {
    for d in [
        cfg.requirements_path(workspace).join("stakeholder"),
        cfg.requirements_path(workspace).join("system"),
        cfg.skills_path(workspace),
    ] {
        std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    }
    Ok(())
}

const DEFAULT_TASK: &str =
    "Satisfy the active requirements in .genji/requirements/. Derive system requirements and tickets as needed.";

/// Read the instruction supplied on the command line. Returns `None` when the
/// user gave neither a task nor a non-empty instructions file, in which case
/// genji should wait for one over the control socket.
fn read_task(instructions_file: Option<&str>, task: Option<&str>) -> Result<Option<String>> {
    if let Some(f) = instructions_file {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading instructions file {f}"))?;
        if !text.trim().is_empty() {
            return Ok(Some(text));
        }
    }
    if let Some(t) = task {
        if !t.trim().is_empty() {
            return Ok(Some(t.to_string()));
        }
    }
    Ok(None)
}

/// What a top-level run should do once it has resolved its inputs.
#[derive(Debug, PartialEq, Eq)]
enum Startup {
    /// Begin work with this task.
    Run(String),
    /// Wait for an instruction over the control socket.
    Wait,
}

/// Decide how to start from the two inputs the user can provide: an explicit
/// instruction and the presence of active requirements. An explicit instruction
/// always wins; otherwise active requirements are the work queue; otherwise
/// nothing was provided and we wait for an instruction.
fn startup_action(explicit_task: Option<String>, active_requirements: i64) -> Startup {
    if let Some(t) = explicit_task {
        return Startup::Run(t);
    }
    if active_requirements > 0 {
        return Startup::Run(DEFAULT_TASK.to_string());
    }
    Startup::Wait
}

/// Block until the user injects an instruction over the control socket (or asks
/// to stop). Returns `None` if a stop was requested before any instruction.
fn wait_for_instruction(control: &control::Control, id: &str, quiet: bool) -> Result<Option<String>> {
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

fn mode_switch_instruction(mode: Mode) -> String {
    match mode {
        Mode::Plan => "Now run in PLAN mode: assess progress against the requirements, update requirements/tickets, and stop when the plan is current.".into(),
        Mode::Build => "Now run in BUILD mode: work the open tickets, verify your changes, and resolve tickets when done.".into(),
        Mode::Explore => "Now run in EXPLORE mode: investigate and report findings.".into(),
        Mode::Retro => "Now run in RETRO mode: analyze history and improve prompts/skills.".into(),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_single(
    cfg: Config,
    workspace: PathBuf,
    db: Db,
    instance_id: String,
    mode: Mode,
    task: String,
    parent: Option<String>,
    depth: u32,
    interactive: bool,
    quiet: bool,
    control: Option<Arc<control::Control>>,
) -> Result<String> {
    let model = cfg.model_for_mode(mode);
    if !quiet {
        eprintln!(
            "[genji] mode={} model={} instance={} task={}",
            mode.as_str(),
            model,
            instance_id,
            llm::truncate(task.clone(), 120)
        );
    }
    let workspace_str = workspace.display().to_string();
    // stdout is always the machine event stream; stderr carries human logs. The
    // same events are appended to a per-instance trace file for `genji inspect`.
    let events = Arc::new(EventEmitter::new(
        instance_id.clone(),
        Some(registry::events_dir().join(format!("{instance_id}.jsonl"))),
    ));
    events.instance_start(
        &workspace_str,
        mode.as_str(),
        &model,
        parent.as_deref(),
        depth,
        &task,
    );
    let mut agent = Agent::new(
        cfg,
        workspace,
        db,
        instance_id,
        parent,
        mode,
        model,
        depth,
        &task,
        interactive,
        control,
        events.clone(),
    )?;
    agent.add_user(&task)?;
    let report = agent.run_loop()?;
    agent.finish("done", &report)?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn run_cycle(
    cfg: Config,
    workspace: PathBuf,
    db: Db,
    instance_id: String,
    start_mode: Mode,
    task: String,
    interactive: bool,
    quiet: bool,
    control: Option<Arc<control::Control>>,
) -> Result<String> {
    let model = cfg.model_for_mode(start_mode);
    if !quiet {
        eprintln!(
            "[genji] auto-cycle instance={} start={} max_cycles={}",
            instance_id,
            start_mode.as_str(),
            cfg.max_cycles
        );
    }
    let max_cycles = cfg.max_cycles;
    let workspace_str = workspace.display().to_string();
    let events = Arc::new(EventEmitter::new(
        instance_id.clone(),
        Some(registry::events_dir().join(format!("{instance_id}.jsonl"))),
    ));
    events.instance_start(&workspace_str, start_mode.as_str(), &model, None, 0, &task);
    let mut agent = Agent::new(
        cfg,
        workspace,
        db,
        instance_id,
        None,
        start_mode,
        model,
        0,
        &task,
        interactive,
        control,
        events.clone(),
    )?;
    agent.add_user(&task)?;

    let mut current = start_mode;
    let mut last_report = String::new();
    for cycle in 0..max_cycles {
        let active = reqmd::active_count(&agent.cfg, &agent.workspace)?;
        events.cycle(cycle + 1, max_cycles, current.as_str(), active);
        if cycle > 0 && active == 0 {
            last_report = format!(
                "All requirements are met (0 active). Stopped after {cycle} cycle(s)."
            );
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
        current = match current {
            Mode::Plan => Mode::Build,
            Mode::Build => Mode::Plan,
            _ => Mode::Plan,
        };
    }
    agent.finish("done", &last_report)?;
    Ok(last_report)
}

/// Removes this process's instance record when the run ends (including early
/// returns and panics during unwinding).
struct InstanceGuard(registry::Instance);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        registry::remove(&self.0.id);
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
    match control::send(Path::new(socket), "/status") {
        Ok(r) => status_text(&r),
        Err(e) => format!("(unreachable: {e:#})"),
    }
}

/// `genji list` — running instances with their live status. stdout is machine
/// output (JSON); the human table is written to stderr.
fn cmd_list() -> Result<()> {
    // Probe each control socket once and reuse the status for both the machine
    // output and the human table.
    let rows: Vec<(registry::Instance, String)> = registry::list_live()
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
        "{:<8} {:<7} {:<8} {:<38} STATUS",
        "ID", "PID", "UPTIME", "WORKSPACE"
    );
    for (inst, status) in &rows {
        eprintln!(
            "{:<8} {:<7} {:<8} {:<38} {}",
            inst.id,
            inst.pid,
            format_uptime(inst.uptime_secs()),
            inst.workspace,
            status
        );
    }
    Ok(())
}

/// `genji stop [ids...|all]` — graceful stop over the control socket.
fn cmd_stop(ids: &[String]) -> Result<()> {
    let targets: Vec<String> = ids
        .iter()
        .flat_map(|s| s.split(','))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();

    if targets.is_empty() {
        eprintln!("warning: `genji stop` needs one or more instance ids, or `all`");
        let instances = registry::list_live();
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
        registry::list_live()
    } else {
        let mut found = Vec::new();
        for id in &targets {
            match registry::find(id) {
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
        match control::send(Path::new(&inst.control_socket), "/stop") {
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

/// The SQLite database and its WAL/SHM companions.
fn db_files(db_path: &Path) -> [PathBuf; 3] {
    let base = db_path.as_os_str().to_string_lossy();
    [
        db_path.to_path_buf(),
        PathBuf::from(format!("{base}-wal")),
        PathBuf::from(format!("{base}-shm")),
    ]
}

/// `genji reset` — delete the workspace database and recreate an empty one.
///
/// Only the SQLite database (and its WAL/SHM sidecars) is removed; the config,
/// skills and requirements markdown files under `.genji/` are left untouched.
fn cmd_reset(workspace: &Path, assume_yes: bool) -> Result<()> {
    let cfg = Config::load_or_create(workspace)?;
    let db_path = cfg.db_file(workspace);

    // Deleting the database out from under a live instance would split its
    // writes across the old (unlinked) and new files. Refuse instead.
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let here = canon(workspace);
    for inst in registry::list_live() {
        if canon(Path::new(&inst.workspace)) == here {
            bail!(
                "instance {} (pid {}) is running in this workspace; stop it first with `genji stop {}`",
                inst.id,
                inst.pid,
                inst.id
            );
        }
    }

    let existing: Vec<PathBuf> = db_files(&db_path)
        .into_iter()
        .filter(|p| p.exists())
        .collect();

    if existing.is_empty() {
        eprintln!("[reset] no database at {}; nothing to delete", db_path.display());
    } else {
        if !assume_yes {
            if !std::io::stdin().is_terminal() {
                bail!(
                    "refusing to delete {} without confirmation; re-run with --yes",
                    db_path.display()
                );
            }
            eprint!(
                "Delete {} and start over? [y/N] ",
                db_path.display()
            );
            use std::io::Write;
            std::io::stderr().flush().ok();
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
                eprintln!("[reset] aborted");
                return Ok(());
            }
        }
        for p in &existing {
            std::fs::remove_file(p)
                .with_context(|| format!("deleting {}", p.display()))?;
        }
        eprintln!("[reset] deleted {}", db_path.display());
    }

    // Recreate the schema so the next run starts from a clean, ready database.
    let db = Db::open(&db_path)?;
    db.init_schema()?;
    prompts::seed_prompts(&db)?;
    eprintln!("[reset] initialized clean database at {}", db_path.display());

    println!(
        "{}",
        serde_json::to_string(&json!({
            "workspace": workspace.display().to_string(),
            "db": db_path.display().to_string(),
            "deleted": existing
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>(),
        }))?
    );
    Ok(())
}

/// `genji instruct <id> <instruction>` — queue text for a running instance.
/// stdout is machine output (JSON); the human message goes to stderr.
fn cmd_instruct(id: &str, instruction: &str) -> Result<()> {
    if instruction.trim().is_empty() {
        bail!("missing instruction (usage: genji instruct <id> <instruction>)");
    }
    let inst = registry::find(id)?;
    let resp = control::send(Path::new(&inst.control_socket), instruction)?;
    let message = status_text(&resp);
    println!(
        "{}",
        serde_json::to_string(&json!({ "id": inst.id, "message": message }))?
    );
    eprintln!("{message}");
    Ok(())
}

/// Resolve an instance id (exact, else a unique prefix) to its trace file.
fn find_trace(instance: &str) -> Result<std::path::PathBuf> {
    let dir = registry::events_dir();
    let exact = dir.join(format!("{instance}.jsonl"));
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

/// A resolved `inspect`/`follow` target: an instance id and its trace, plus the
/// live instance record when the id named one.
struct TraceTarget {
    instance_id: String,
    trace_path: std::path::PathBuf,
    instance: Option<registry::Instance>,
}

/// Resolve `<id>` as a live instance id, else as a recorded (finished) instance
/// id whose trace still exists.
fn resolve_target(id: &str) -> Result<TraceTarget> {
    let id = id.trim();
    if id.is_empty() {
        bail!("missing instance id");
    }
    if let Ok(inst) = registry::find(id) {
        let trace_path = registry::events_dir().join(format!("{}.jsonl", inst.id));
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

/// Read a target's trace. A missing file is empty for a live instance.
fn read_trace(target: &TraceTarget) -> Result<Option<String>> {
    if target.trace_path.exists() {
        Ok(Some(std::fs::read_to_string(&target.trace_path).with_context(
            || format!("reading event trace {}", target.trace_path.display()),
        )?))
    } else if target.instance.is_some() {
        Ok(None)
    } else {
        bail!("no event trace for instance `{}`", target.instance_id)
    }
}

/// The `instance_start` event of a trace, if present.
fn trace_instance_start(text: &str) -> Option<serde_json::Value> {
    text.lines().find_map(|l| {
        let v: serde_json::Value = serde_json::from_str(l).ok()?;
        (v.get("type").and_then(|t| t.as_str()) == Some("instance_start")).then_some(v)
    })
}

/// True when a trace line is an `instance_end` event.
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

/// `genji inspect <id>` — a brief summary of an instance.
///
/// stdout is a single JSON object; the human-readable view goes to stderr. Use
/// `genji follow <id>` to stream the event trace itself.
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

    let mut obj = json!({
        "type": "instance",
        "id": id.trim(),
        "trace": target.trace_path.display().to_string(),
        "events": count,
        "ended": ended,
    });
    let map = obj.as_object_mut().expect("object");
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

    // Human summary.
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
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Instance-management subcommands are thin clients over the registry/control
    // socket; handle them before loading config or touching the workspace.
    match &cli.command {
        Some(Command::List) => return cmd_list(),
        Some(Command::Stop { ids }) => return cmd_stop(ids),
        Some(Command::Instruct { id, instruction }) => {
            let text = instruction.join(" ");
            return cmd_instruct(id, &text);
        }
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
    if cli.verbose {
        cfg.verbose = true;
    }
    // Provider precedence: --provider > GENJI_PROVIDER > config.provider.
    if let Some(p) = &cli.provider {
        cfg.provider = p.clone();
    } else if let Ok(p) = std::env::var("GENJI_PROVIDER") {
        if !p.trim().is_empty() {
            cfg.provider = p;
        }
    }
    ensure_layout(&cfg, &workspace)?;

    let db = Db::open(&cfg.db_file(&workspace))?;
    db.init_schema()?;

    let synced = tools::skills::sync_skills(&db, &cfg.skills_path(&workspace)).unwrap_or(0);
    if cfg.auto_ingest_requirements {
        let total = reqmd::sync(&db, &cfg, &workspace)?;
        if total > 0 && !cli.quiet_startup {
            eprintln!(
                "[requirements] loaded {total} md file(s) from {}",
                cfg.requirements_path(&workspace).display()
            );
        }
    }
    prompts::seed_prompts(&db)?;
    if synced > 0 && !cli.quiet_startup {
        eprintln!("[skills] synced {synced} skill file(s)");
    }

    if cli.init {
        eprintln!("[genji] initialized workspace at {}", workspace.display());
        eprintln!("  config:       {}", Config::path_in(&workspace).display());
        eprintln!("  skills:       {}", cfg.skills_path(&workspace).display());
        eprintln!("  requirements: {}", cfg.requirements_path(&workspace).display());
        eprintln!("  db:           {}", cfg.db_file(&workspace).display());
        return Ok(());
    }

    let interactive = cli.interactive
        || (!cli.subagent && std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
    // With no mode subcommand we default to build mode. Auto-cycling is implied
    // in that case, and `--cycle` enables it for any explicit starting mode.
    let (start_mode, task_arg): (Mode, Option<&str>) = match &cli.command {
        Some(Command::Plan { task }) => (Mode::Plan, task.as_deref()),
        Some(Command::Build { task }) => (Mode::Build, task.as_deref()),
        Some(Command::Explore { task }) => (Mode::Explore, task.as_deref()),
        Some(Command::Retro { task }) => (Mode::Retro, task.as_deref()),
        Some(_) => unreachable!("instance subcommand handled above"),
        None => (Mode::Build, cli.task.as_deref()),
    };
    let cycle = cli.command.is_none() || cli.cycle;
    let explicit_task = read_task(cli.instructions_file.as_deref(), task_arg)?;
    let quiet = cli.quiet_startup || cli.subagent;
    // One id per run: the instance id. It names the registry record, the event
    // trace that `genji inspect` reads, and the DB record, and every event is
    // tagged with it. Subagents get one too even though they do not register.
    let instance_id = registry::new_id();
    // Materialise the trace up front so `genji inspect` works even before the
    // first event (for example while an instance waits for an instruction).
    {
        let trace = registry::events_dir().join(format!("{instance_id}.jsonl"));
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
    // Per-model limits (token budget, context window, max output) are resolved
    // for the active model via `cfg.limits_for_model`, so a profile or model
    // switch carries its own budget. Nothing to patch globally here.

    // Top-level runs open a control socket so instructions can be injected
    // mid-run; subagents never do. Controllable runs also register themselves so
    // `genji list`/`stop`/`instruct`/`inspect` can find them from anywhere.
    let mut _instance_guard: Option<InstanceGuard> = None;
    let control = if cli.no_control || !cfg.control_enabled {
        None
    } else {
        let c = control::Control::start(cfg.control_path(&workspace))?;
        if !quiet {
            eprintln!("[control] listening on {}", c.path.display());
        }
        let inst = registry::Instance {
            id: instance_id.clone(),
            pid: std::process::id(),
            workspace: workspace.display().to_string(),
            control_socket: c.path.display().to_string(),
            label: cli.label.clone(),
            started_at: registry::now_secs(),
        };
        if let Err(e) = inst.save() {
            eprintln!("[registry] warning: could not register instance: {e:#}");
        }
        _instance_guard = Some(InstanceGuard(inst));
        Some(c)
    };

    // Start from an explicit instruction when given, else the active
    // requirements, else wait for an instruction on the control socket.
    let task = match startup_action(explicit_task, reqmd::active_count(&cfg, &workspace)?) {
        Startup::Run(t) => t,
        Startup::Wait => match &control {
            Some(c) => match wait_for_instruction(c, &instance_id, quiet)? {
                Some(t) => t,
                None => {
                    c.shutdown();
                    return Ok(());
                }
            },
            // No control socket to wait on and no work to do: there is no way
            // to receive an instruction, so exit cleanly.
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

    let report = if cycle {
        run_cycle(
            cfg,
            workspace,
            db,
            instance_id,
            start_mode,
            task,
            interactive,
            quiet,
            control.clone(),
        )?
    } else {
        let parent = cli.parent_instance.clone();
        run_single(
            cfg,
            workspace,
            db,
            instance_id,
            start_mode,
            task,
            parent,
            cli.depth,
            interactive,
            quiet,
            control.clone(),
        )?
    };

    if let Some(c) = &control {
        c.shutdown();
    }
    // stdout carries the machine event stream; the final report is delivered in
    // the `instance_end` event. Mirror it to stderr so humans still see the
    // answer without having to parse the events.
    eprintln!("[report] {report}");
    Ok(())
}
