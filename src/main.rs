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

use agent::{Agent, AgentParams};
use config::Config;
use serde_json::{Value, json};
use storage::db::Db;
use storage::modes::Mode;
use storage::registry;

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
    /// Make an instance follow plan `<slug>`: plan mode updates it (creating it
    /// when missing), build mode follows it (a missing plan is a no-op).
    Setplan {
        /// Instance id (see `genji list`).
        id: String,
        /// Plan slug (the `<slug>.md` file under the plans directory).
        slug: String,
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

impl Cli {
    /// Whether formal (requirements + tickets) mode is on; always off without the feature.
    fn formal(&self) -> bool {
        #[cfg(feature = "formal")]
        {
            self.formal
        }
        #[cfg(not(feature = "formal"))]
        {
            false
        }
    }
}

fn resolve_workspace(cli: &Cli) -> Result<PathBuf> {
    match &cli.workspace {
        Some(workspace) => Ok(PathBuf::from(workspace)),
        None => std::env::current_dir().context("resolving current directory"),
    }
}

fn apply_config_overrides(cfg: &mut Config, cli: &Cli) {
    // Provider precedence: --provider > GENJI_PROVIDER > config.provider.
    if let Some(provider) = &cli.provider {
        cfg.provider.clone_from(provider);
    } else if let Ok(provider) = std::env::var("GENJI_PROVIDER")
        && !provider.trim().is_empty()
    {
        cfg.provider = provider;
    }
}

impl Command {
    /// The agent mode and optional task for the agent-running commands.
    fn mode(&self) -> Option<(Mode, Option<&str>)> {
        match self {
            Self::Plan { task } => Some((Mode::Plan, task.as_deref())),
            Self::Build { task } => Some((Mode::Build, task.as_deref())),
            Self::Explore { task } => Some((Mode::Explore, task.as_deref())),
            Self::Retro { task } => Some((Mode::Retro, task.as_deref())),
            _ => None,
        }
    }

    /// Run a management command. Returns `false` for the agent-running modes.
    fn execute(&self, cli: &Cli) -> Result<bool> {
        match self {
            Self::List => cmd_list()?,
            Self::Stop { ids } => cmd_stop(ids)?,
            Self::Instruct { id, instruction } => cmd_instruct(id, &instruction.join(" "))?,
            Self::Setplan { id, slug } => cmd_setplan(id, slug)?,
            Self::Inspect { id } => cmd_inspect(id, cli)?,
            Self::Reset { yes } => cmd_reset(&resolve_workspace(cli)?, *yes)?,
            _ => return Ok(false),
        }
        Ok(true)
    }
}

fn prepare_workspace(workspace: &Path, cli: &Cli) -> Result<(Config, Db, bool)> {
    let mut cfg = Config::load_or_create(workspace)?;
    apply_config_overrides(&mut cfg, cli);
    let formal = cli.formal();
    for d in cfg.layout_dirs(workspace, formal) {
        std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    }
    let db = Db::open(&cfg.db_file(workspace))?;
    #[cfg(feature = "formal")]
    if formal {
        if cfg.auto_ingest_requirements {
            let total = storage::reqmd::load_all(&cfg, workspace)?.len();
            if total > 0 && !cli.quiet_startup {
                eprintln!(
                    "[requirements] loaded {total} md file(s) from {}",
                    cfg.requirements_path(workspace).display()
                );
            }
        }
        storage::ticketmd::load_all(&cfg, workspace)?;
    }
    Ok((cfg, db, formal))
}

const DEFAULT_TASK: &str = "Satisfy the active requirements in .genji/requirements/. Derive system requirements and tickets as needed.";

/// Whether formal mode is on and at least one requirement is still active.
fn has_active_requirements(cfg: &Config, workspace: &Path, formal: bool) -> Result<bool> {
    #[cfg(feature = "formal")]
    if formal {
        return Ok(storage::reqmd::active_count(cfg, workspace)? > 0);
    }
    let _ = (cfg, workspace, formal);
    Ok(false)
}

fn read_task(instructions_file: Option<&str>, task: Option<&str>) -> Result<Option<String>> {
    if let Some(f) = instructions_file {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading instructions file {f}"))?;
        if !text.trim().is_empty() {
            return Ok(Some(text));
        }
    }
    Ok(task.filter(|t| !t.trim().is_empty()).map(str::to_string))
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
    let instructions = control.wait_for_instruction();
    if instructions.is_none() && !quiet {
        eprintln!("[genji] stop requested before any instruction; exiting");
    }
    Ok(instructions.map(|queued| queued.join("\n")))
}

#[cfg(feature = "formal")]
fn mode_switch_instruction(mode: Mode) -> &'static str {
    match mode {
        Mode::Plan => {
            "Now run in PLAN mode: assess progress against the requirements, update requirements/tickets, and stop when the plan is current."
        }
        Mode::Build => {
            "Now run in BUILD mode: work the open tickets, verify your changes, and resolve tickets when done."
        }
        Mode::Explore => "Now run in EXPLORE mode: investigate and report findings.",
        Mode::Retro => "Now run in RETRO mode: analyze history and improve prompts/skills.",
    }
}

fn run_single(params: AgentParams, quiet: bool) -> Result<String> {
    let task = params.task.clone();
    let mut agent = Agent::new(params)?;
    if !quiet {
        eprintln!(
            "[genji] mode={} model={} instance={} task={}",
            agent.mode,
            agent.llm.model,
            agent.instance_id,
            llm::truncate(&task, 120)
        );
    }
    agent.add_user(&task)?;
    let report = agent.run_loop()?;
    agent.finish(agent.status(), &report)?;
    Ok(report)
}

/// Alternate plan and build until no requirement is active, the cycle budget
/// runs out, the user stops the run, or the LLM fails.
#[cfg(feature = "formal")]
fn run_cycle(params: AgentParams, quiet: bool) -> Result<String> {
    let task = params.task.clone();
    let mut current = params.mode;
    let max_cycles = params.cfg.max_cycles;
    let mut agent = Agent::new(params)?;
    if !quiet {
        eprintln!(
            "[genji] auto-cycle instance={} start={current} max_cycles={max_cycles}",
            agent.instance_id
        );
    }
    agent.add_user(&task)?;

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
                "[cycle {}/{max_cycles}] mode={current} active_requirements={active} tokens={}",
                cycle + 1,
                agent.tokens_used
            );
        }
        agent.set_mode(current)?;
        if cycle > 0 {
            agent.add_user(mode_switch_instruction(current))?;
        }
        last_report = agent.run_loop()?;
        if !quiet {
            eprintln!(
                "[cycle {}] {current} done: {}",
                cycle + 1,
                llm::truncate(&last_report, 300)
            );
        }
        if agent.control.as_ref().is_some_and(|c| c.stop_requested()) {
            eprintln!("[cycle] stop requested; ending cycle");
            break;
        }
        if agent.failed {
            eprintln!("[cycle] LLM failure; ending cycle");
            break;
        }
        current = if current == Mode::Plan {
            Mode::Build
        } else {
            Mode::Plan
        };
    }
    agent.finish(agent.status(), &last_report)?;
    Ok(last_report)
}

/// Keeps the control socket and registry entry alive; removes both on drop.
struct InstanceGuard {
    instance: registry::Instance,
    control: std::sync::Arc<socket::Control>,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        self.control.shutdown();
        registry::remove(&self.instance.id);
    }
}

fn start_control(
    cfg: &Config,
    workspace: &Path,
    context: std::sync::Arc<std::sync::RwLock<storage::context::ContextComposer>>,
    instance_id: &str,
    label: String,
    quiet: bool,
) -> Result<InstanceGuard> {
    let control = socket::Control::start(
        cfg.control_path(workspace),
        cfg.plans_path(workspace),
        context,
    )?;
    if !quiet {
        eprintln!("[control] listening on {}", control.path.display());
    }
    let instance = registry::Instance {
        id: instance_id.to_string(),
        pid: std::process::id(),
        workspace: workspace.display().to_string(),
        control_socket: control.path.display().to_string(),
        label,
        started_at: storage::util::unix_secs(),
    };
    if let Err(e) = instance.save() {
        eprintln!("[registry] warning: could not register instance: {e:#}");
    }
    Ok(InstanceGuard { instance, control })
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

/// `genji list` — running instances with their live status. stdout is machine
/// output (JSON); the human table is written to stderr.
fn cmd_list() -> Result<()> {
    let rows = registry::list_live();
    let arr: Vec<Value> = rows
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
            "{:<8} {:<7} {:<8} {:<38} {status}",
            inst.id,
            inst.pid,
            format_uptime(inst.uptime_secs()),
            inst.workspace,
        );
    }
    Ok(())
}

fn cmd_stop(ids: &[String]) -> Result<()> {
    let targets: Vec<&str> = ids
        .iter()
        .flat_map(|s| s.split(','))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if targets.is_empty() {
        eprintln!("warning: `genji stop` needs one or more instance ids, or `all`");
        let instances = registry::list_live();
        if instances.is_empty() {
            eprintln!("no running genji instances");
        } else {
            eprintln!("running instances:");
            for (inst, _) in &instances {
                let label = if inst.label.is_empty() {
                    String::new()
                } else {
                    format!("  ({})", inst.label)
                };
                eprintln!("  {}  pid={}  {}{label}", inst.id, inst.pid, inst.workspace);
            }
            eprintln!("use `genji stop all` or `genji stop <id>...`");
        }
        std::process::exit(2);
    }

    let mut failed = false;
    let instances: Vec<registry::Instance> = if targets.contains(&"all") {
        registry::list_live()
            .into_iter()
            .map(|(inst, _)| inst)
            .collect()
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
    if instances.is_empty() && !failed {
        eprintln!("no running genji instances");
    }
    let results: Vec<Value> = instances
        .iter()
        .map(|inst| {
            let (ok, message) = match socket::send(Path::new(&inst.control_socket), "/stop") {
                Ok(r) => {
                    eprintln!("stopping {} (pid {}): {r}", inst.id, inst.pid);
                    (true, r)
                }
                Err(e) => {
                    eprintln!("failed to stop {}: {e:#}", inst.id);
                    failed = true;
                    (false, format!("{e:#}"))
                }
            };
            json!({ "id": inst.id, "pid": inst.pid, "ok": ok, "message": message })
        })
        .collect();
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

/// `genji reset` — wipe the workspace database and the plans (and, with the
/// `formal` feature, requirements and tickets) trees, then recreate an empty
/// database. Config, skills and prompts are left untouched. The user is told how
/// many files will be destroyed before anything is deleted.
fn cmd_reset(workspace: &Path, assume_yes: bool) -> Result<()> {
    let cfg = Config::load_or_create(workspace)?;
    let db_path = cfg.db_file(workspace);
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let here = canon(workspace);
    for (inst, _) in registry::list_live() {
        if canon(Path::new(&inst.workspace)) == here {
            bail!(
                "instance {} (pid {}) is running in this workspace; stop it first with `genji stop {}`",
                inst.id,
                inst.pid,
                inst.id
            );
        }
    }
    let mut dirs = vec![("plan(s)", cfg.plans_path(workspace))];
    dirs.extend(cfg.formal_dirs(workspace, true));
    let db_existing: Vec<PathBuf> = db_files(&db_path)
        .into_iter()
        .filter(|p| p.exists())
        .collect();
    let dir_files: Vec<Vec<PathBuf>> = dirs
        .iter()
        .map(|(_, dir)| {
            let mut files = Vec::new();
            collect_files(dir, &mut files);
            files
        })
        .collect();
    let total = db_existing.len() + dir_files.iter().map(Vec::len).sum::<usize>();

    if total == 0 {
        eprintln!("[reset] nothing to delete; database and content directories are already empty");
    } else {
        let mut summary = vec![format!("{} database file(s)", db_existing.len())];
        summary.extend(
            dirs.iter().zip(&dir_files).map(|((label, dir), files)| {
                format!("{} {label} in {}", files.len(), dir.display())
            }),
        );
        eprintln!(
            "[reset] this will delete {total} file(s): {}",
            summary.join(", ")
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
        for file in &db_existing {
            std::fs::remove_file(file).with_context(|| format!("deleting {}", file.display()))?;
        }
        for (_, dir) in dirs.iter().filter(|(_, dir)| dir.exists()) {
            std::fs::remove_dir_all(dir).with_context(|| format!("deleting {}", dir.display()))?;
        }
        eprintln!("[reset] deleted {total} file(s)");
    }
    for (_, dir) in &dirs {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    Db::open(&db_path)?;
    eprintln!(
        "[reset] initialized clean database at {}",
        db_path.display()
    );
    let deleted: Vec<String> = db_existing
        .iter()
        .chain(dir_files.iter().flatten())
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

/// Send one control-socket line to a live instance and return its reply.
fn send_to(id: &str, msg: &str) -> Result<(registry::Instance, String)> {
    let inst = registry::find(id)?;
    let reply = socket::send(Path::new(&inst.control_socket), msg)?;
    Ok((inst, reply))
}

fn print_message(id: &str, reply: &str, plan: Option<&str>) -> Result<()> {
    let message = reply.strip_prefix("status:").unwrap_or(reply).trim();
    let mut out = json!({ "id": id, "message": message });
    if let Some(plan) = plan {
        out["plan"] = json!(plan);
    }
    println!("{}", serde_json::to_string(&out)?);
    eprintln!("{message}");
    Ok(())
}

fn cmd_instruct(id: &str, instruction: &str) -> Result<()> {
    if instruction.trim().is_empty() {
        bail!("missing instruction (usage: genji instruct <id> <instruction>)");
    }
    let (inst, reply) = send_to(id, instruction)?;
    // Structured replies (e.g. `/context`) are passed through unchanged.
    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(&reply) {
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    print_message(&inst.id, &reply, None)
}

fn cmd_setplan(id: &str, slug: &str) -> Result<()> {
    if slug.trim().is_empty() {
        bail!("missing plan slug (usage: genji setplan <id> <slug>)");
    }
    let (inst, reply) = send_to(id, &format!("/setplan {slug}"))?;
    print_message(&inst.id, &reply, Some(slug))
}

/// `genji inspect` — recorded summary of an instance from the workspace
/// database, plus live details when it is still running.
fn cmd_inspect(id: &str, cli: &Cli) -> Result<()> {
    let id = id.trim();
    let live = registry::find(id).ok();
    let workspace = match &live {
        Some(inst) => PathBuf::from(&inst.workspace),
        None => resolve_workspace(cli)?,
    };
    let cfg = Config::load_or_create(&workspace)?;
    let db = Db::open(&cfg.db_file(&workspace))?;
    let mut obj = db.instance_summary(live.as_ref().map_or(id, |i| i.id.as_str()))?;
    if let (Some(map), Some(inst)) = (obj.as_object_mut(), &live) {
        map.insert("pid".into(), json!(inst.pid));
        map.insert("label".into(), json!(inst.label));
        map.insert("workspace".into(), json!(inst.workspace));
        map.insert("control_socket".into(), json!(inst.control_socket));
        map.insert("uptime_secs".into(), json!(inst.uptime_secs()));
        map.insert("live_status".into(), json!(inst.status()));
    }
    println!("{}", serde_json::to_string(&obj)?);
    for key in [
        "id",
        "mode",
        "model",
        "parent",
        "depth",
        "task",
        "status",
        "live_status",
        "tokens_used",
        "messages",
        "started_at",
        "ended_at",
        "pid",
        "uptime_secs",
        "control_socket",
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(command) = &cli.command
        && command.execute(&cli)?
    {
        return Ok(());
    }
    let workspace = resolve_workspace(&cli)?;
    let (cfg, db, formal) = prepare_workspace(&workspace, &cli)?;
    let (start_mode, task_arg) = cli
        .command
        .as_ref()
        .and_then(Command::mode)
        .unwrap_or((Mode::Build, cli.task.as_deref()));
    let explicit_task = read_task(cli.instructions_file.as_deref(), task_arg)?;
    let quiet = cli.quiet_startup || cli.subagent;
    let instance_id = registry::new_id();
    let runtime = cfg.runtime_for_mode(start_mode)?;
    if !quiet {
        eprintln!(
            "[genji] provider={} kind={} base_url={} model={}",
            cfg.provider, runtime.provider.kind, runtime.provider.base_url, runtime.model
        );
    }
    let context = agent::build_context(
        &cfg,
        &workspace,
        start_mode,
        formal,
        runtime.limits.context_window,
    );

    // Top-level runs open a control socket so instructions can be injected
    // mid-run; subagents never do. Controllable runs also register themselves so
    // `genji list`/`stop`/`instruct`/`inspect` can find them from anywhere.
    let guard = if cli.no_control || !cfg.control_enabled {
        None
    } else {
        Some(start_control(
            &cfg,
            &workspace,
            context.clone(),
            &instance_id,
            cli.label.clone(),
            quiet,
        )?)
    };
    let control = guard.as_ref().map(|g| g.control.clone());

    let task = if let Some(t) = explicit_task {
        t
    } else {
        if has_active_requirements(&cfg, &workspace, formal)? {
            DEFAULT_TASK.to_string()
        } else if let Some(c) = &control {
            match wait_for_instruction(c, &instance_id, quiet)? {
                Some(t) => t,
                None => return Ok(()),
            }
        } else {
            if !quiet {
                eprintln!(
                    "[genji] no instruction and no active requirements; \
                     no control socket to wait on. Nothing to do."
                );
            }
            return Ok(());
        }
    };

    let params = AgentParams {
        cfg,
        workspace,
        db,
        instance_id,
        parent_instance: cli.parent_instance.clone(),
        mode: start_mode,
        depth: cli.depth,
        task,
        formal,
        control,
        context,
        runtime,
    };
    #[cfg(feature = "formal")]
    let report = if formal && !cli.subagent {
        run_cycle(params, quiet)?
    } else {
        run_single(params, quiet)?
    };
    #[cfg(not(feature = "formal"))]
    let report = run_single(params, quiet)?;

    eprintln!("[report] {report}");
    Ok(())
}
