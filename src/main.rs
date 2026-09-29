mod agent;
mod config;
mod control;
mod db;
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agent::Agent;
use config::Config;
use db::Db;
use modes::Mode;

static SESSION_COUNTER: AtomicU64 = AtomicU64::new(0);

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
    parent_session: Option<String>,
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
    /// Show brief stats for a running genji instance.
    Inspect {
        /// Instance id (see `genji list`).
        id: String,
    },
}

fn new_session_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let c = SESSION_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}-{:x}", nanos, std::process::id(), c)
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
    control.set_status("idle (waiting for instruction)");
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
    mode: Mode,
    task: String,
    parent: Option<String>,
    depth: u32,
    interactive: bool,
    quiet: bool,
    control: Option<Arc<control::Control>>,
) -> Result<String> {
    let model = cfg.model_for_mode(mode);
    let session_id = new_session_id();
    if !quiet {
        eprintln!(
            "[genji] mode={} model={} session={} task={}",
            mode.as_str(),
            model,
            session_id,
            llm::truncate(task.clone(), 120)
        );
    }
    let mut agent = Agent::new(
        cfg,
        workspace,
        db,
        session_id,
        parent,
        mode,
        model,
        depth,
        &task,
        interactive,
        control,
    )?;
    agent.add_user(&task)?;
    let report = agent.run_loop()?;
    agent.finish("done", &report)?;
    Ok(report)
}

fn run_cycle(
    cfg: Config,
    workspace: PathBuf,
    db: Db,
    start_mode: Mode,
    task: String,
    interactive: bool,
    quiet: bool,
    control: Option<Arc<control::Control>>,
) -> Result<String> {
    let session_id = new_session_id();
    let model = cfg.model_for_mode(start_mode);
    if !quiet {
        eprintln!(
            "[genji] auto-cycle session={} start={} max_cycles={}",
            session_id,
            start_mode.as_str(),
            cfg.max_cycles
        );
    }
    let max_cycles = cfg.max_cycles;
    let mut agent = Agent::new(
        cfg,
        workspace,
        db,
        session_id,
        None,
        start_mode,
        model,
        0,
        &task,
        interactive,
        control,
    )?;
    agent.add_user(&task)?;

    let mut current = start_mode;
    let mut last_report = String::new();
    for cycle in 0..max_cycles {
        let active = reqmd::active_count(&agent.cfg, &agent.workspace)?;
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

/// `genji list` — running instances with their live status.
fn cmd_list() -> Result<()> {
    let instances = registry::list_live();
    if instances.is_empty() {
        println!("no running genji instances");
        return Ok(());
    }
    println!(
        "{:<8} {:<7} {:<8} {:<38} STATUS",
        "ID", "PID", "UPTIME", "WORKSPACE"
    );
    for inst in &instances {
        println!(
            "{:<8} {:<7} {:<8} {:<38} {}",
            inst.id,
            inst.pid,
            format_uptime(inst.uptime_secs()),
            inst.workspace,
            query_status(&inst.control_socket)
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

    if instances.is_empty() {
        if !failed {
            println!("no running genji instances");
        }
        if failed {
            std::process::exit(1);
        }
        return Ok(());
    }
    for inst in &instances {
        match control::send(Path::new(&inst.control_socket), "/stop") {
            Ok(r) => println!("stopping {} (pid {}): {}", inst.id, inst.pid, r),
            Err(e) => {
                eprintln!("failed to stop {}: {e:#}", inst.id);
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

/// `genji instruct <id> <instruction>` — queue text for a running instance.
fn cmd_instruct(id: &str, instruction: &str) -> Result<()> {
    if instruction.trim().is_empty() {
        bail!("missing instruction (usage: genji instruct <id> <instruction>)");
    }
    let inst = registry::find(id)?;
    let resp = control::send(Path::new(&inst.control_socket), instruction)?;
    println!("{}", status_text(&resp));
    Ok(())
}

/// `genji inspect <id>` — metadata plus live status for one instance.
fn cmd_inspect(id: &str) -> Result<()> {
    let inst = registry::find(id)?;
    println!("id:        {}", inst.id);
    println!("pid:       {}", inst.pid);
    if !inst.label.is_empty() {
        println!("label:     {}", inst.label);
    }
    println!("workspace: {}", inst.workspace);
    println!("socket:    {}", inst.control_socket);
    println!("uptime:    {}", format_uptime(inst.uptime_secs()));
    println!("status:    {}", query_status(&inst.control_socket));
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
    let mut instance_id: Option<String> = None;
    let control = if cli.no_control || !cfg.control_enabled {
        None
    } else {
        let c = control::Control::start(cfg.control_path(&workspace))?;
        if !quiet {
            eprintln!("[control] listening on {}", c.path.display());
        }
        let id = registry::new_id();
        let inst = registry::Instance {
            id: id.clone(),
            pid: std::process::id(),
            workspace: workspace.display().to_string(),
            control_socket: c.path.display().to_string(),
            label: cli.label.clone(),
            started_at: registry::now_secs(),
        };
        if let Err(e) = inst.save() {
            eprintln!("[registry] warning: could not register instance: {e:#}");
        }
        instance_id = Some(id);
        _instance_guard = Some(InstanceGuard(inst));
        Some(c)
    };

    // Start from an explicit instruction when given, else the active
    // requirements, else wait for an instruction on the control socket.
    let task = match startup_action(explicit_task, reqmd::active_count(&cfg, &workspace)?) {
        Startup::Run(t) => t,
        Startup::Wait => match &control {
            Some(c) => match wait_for_instruction(c, instance_id.as_deref().unwrap_or(""), quiet)? {
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
            start_mode,
            task,
            interactive,
            quiet,
            control.clone(),
        )?
    } else {
        let parent = cli.parent_session.clone();
        run_single(
            cfg,
            workspace,
            db,
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
    println!("{report}");
    Ok(())
}
