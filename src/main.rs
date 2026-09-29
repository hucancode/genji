mod agent;
mod config;
mod control;
mod db;
mod llm;
mod modes;
mod proc;
mod prompts;
mod reqmd;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;
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
    about = "A minimal pi-like coding agent (sqlite tickets/requirements, modes, subagents, retro)",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    /// Mode to run in. Omit to auto-cycle plan -> build until all requirements are met.
    #[command(subcommand)]
    command: Option<Command>,

    /// The user request / task (used when no mode subcommand is given).
    task: Option<String>,

    /// Automatically cycle plan -> build -> plan until all requirements are met.
    #[arg(long, global = true)]
    cycle: bool,

    /// Internal: run as a spawned subagent (single mode, report to stdout).
    #[arg(long, hide = true, global = true)]
    subagent: bool,

    #[arg(long, hide = true, global = true)]
    parent_session: Option<String>,

    #[arg(long, hide = true, global = true)]
    instructions_file: Option<String>,

    /// Internal task label for the session record.
    #[arg(long, default_value = "", global = true)]
    label: String,

    #[arg(long, default_value_t = 0, hide = true, global = true)]
    depth: u32,

    #[arg(long, global = true)]
    quiet_startup: bool,

    #[arg(long, global = true)]
    verbose: bool,

    /// Allow interactive questions (requirement_ask). Defaults to on for TTY runs.
    #[arg(long, global = true)]
    interactive: bool,

    /// Workspace root (defaults to current directory).
    #[arg(long, global = true)]
    workspace: Option<String>,

    /// Select a provider profile (a key of `providers` in the config).
    /// Overrides the config; `GENJI_PROVIDER` is used when this is absent.
    #[arg(long, global = true)]
    provider: Option<String>,

    /// Create config/prompt/skill/requirement scaffolding and exit.
    #[arg(long, global = true)]
    init: bool,

    /// Send an instruction to a running agent's control socket, then exit.
    #[arg(long, value_name = "TEXT", global = true)]
    send: Option<String>,

    /// Ask a running agent to stop gracefully, then exit.
    #[arg(long, global = true)]
    stop: bool,

    /// Print a running agent's status, then exit.
    #[arg(long, global = true)]
    status: bool,

    /// Internal: do not open a control socket (used by subagents).
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
        cfg.prompts_path(workspace),
    ] {
        std::fs::create_dir_all(&d).with_context(|| format!("creating {}", d.display()))?;
    }
    Ok(())
}

fn read_task(instructions_file: Option<&str>, task: Option<&str>) -> Result<String> {
    if let Some(f) = instructions_file {
        let text =
            std::fs::read_to_string(f).with_context(|| format!("reading instructions file {f}"))?;
        if !text.trim().is_empty() {
            return Ok(text);
        }
    }
    if let Some(t) = task {
        return Ok(t.to_string());
    }
    Ok("Satisfy the active requirements in the database. Derive system requirements and tickets as needed.".into())
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
    task: String,
    interactive: bool,
    quiet: bool,
    control: Option<Arc<control::Control>>,
) -> Result<String> {
    let session_id = new_session_id();
    let start_mode = Mode::Plan;
    let model = cfg.model_for_mode(start_mode);
    if !quiet {
        eprintln!(
            "[genji] auto-cycle session={} start=plan max_cycles={}",
            session_id, cfg.max_cycles
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

    let mut current = Mode::Plan;
    let mut last_report = String::new();
    for cycle in 0..max_cycles {
        let active = agent.db.requirement_active_count()?;
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

fn main() -> Result<()> {
    let cli = Cli::parse();
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

    // Client commands: talk to an already-running agent over its control socket.
    if cli.send.is_some() || cli.stop || cli.status {
        let path = cfg.control_path(&workspace);
        let msg = if let Some(t) = &cli.send {
            t.clone()
        } else if cli.stop {
            "/stop".to_string()
        } else {
            "/status".to_string()
        };
        println!("{}", control::send(&path, &msg)?);
        return Ok(());
    }

    let db = Db::open(&cfg.db_file(&workspace))?;
    db.init_schema()?;

    let synced = tools::skills::sync_skills(&db, &cfg.skills_path(&workspace)).unwrap_or(0);
    if cfg.auto_ingest_requirements {
        let (total, changed) = reqmd::sync_requirements_md(&db, &cfg, &workspace)?;
        if total > 0 && !cli.quiet_startup {
            eprintln!("[requirements] ingested {total} md file(s), {changed} changed");
        }
    }
    prompts::seed_prompts(&db, &cfg, &workspace)?;
    if synced > 0 && !cli.quiet_startup {
        eprintln!("[skills] synced {synced} skill file(s)");
    }

    if cli.init {
        eprintln!("[genji] initialized workspace at {}", workspace.display());
        eprintln!("  config:       {}", Config::path_in(&workspace).display());
        eprintln!("  prompts:      {}", cfg.prompts_path(&workspace).display());
        eprintln!("  skills:       {}", cfg.skills_path(&workspace).display());
        eprintln!("  requirements: {}", cfg.requirements_path(&workspace).display());
        eprintln!("  db:           {}", cfg.db_file(&workspace).display());
        return Ok(());
    }

    let interactive = cli.interactive
        || (!cli.subagent && std::io::stdin().is_terminal() && std::io::stdout().is_terminal());
    let (mode, task_arg): (Option<Mode>, Option<&str>) = match &cli.command {
        Some(Command::Plan { task }) => (Some(Mode::Plan), task.as_deref()),
        Some(Command::Build { task }) => (Some(Mode::Build), task.as_deref()),
        Some(Command::Explore { task }) => (Some(Mode::Explore), task.as_deref()),
        Some(Command::Retro { task }) => (Some(Mode::Retro), task.as_deref()),
        None => (None, cli.task.as_deref()),
    };
    let task = read_task(cli.instructions_file.as_deref(), task_arg)?;
    let quiet = cli.quiet_startup || cli.subagent;

    if !quiet {
        let p = cfg.resolve_active_provider();
        eprintln!(
            "[genji] provider={} kind={} base_url={} model={}",
            cfg.provider,
            p.kind,
            p.base_url,
            cfg.model_for_mode(Mode::Plan)
        );
    }
    // A provider profile may override model limits (e.g. a small local context).
    {
        let p = cfg.resolve_active_provider();
        if let Some(cw) = p.context_window {
            cfg.context_window = cw;
        }
        if let Some(mo) = p.max_output_tokens {
            cfg.max_output_tokens = mo;
        }
    }

    // Top-level runs open a control socket so instructions can be injected
    // mid-run; subagents never do.
    let control = if cli.no_control || !cfg.control_enabled {
        None
    } else {
        let c = control::Control::start(cfg.control_path(&workspace))?;
        if !quiet {
            eprintln!("[control] listening on {}", c.path.display());
        }
        Some(c)
    };

    let report = match mode {
        Some(mode) => {
            let parent = cli.parent_session.clone();
            run_single(
                cfg,
                workspace,
                db,
                mode,
                task,
                parent,
                cli.depth,
                interactive,
                quiet,
                control.clone(),
            )?
        }
        None => {
            // Default behaviour: automatic plan->build cycling. `--cycle` is
            // accepted explicitly for clarity.
            let _ = cli.cycle;
            run_cycle(cfg, workspace, db, task, interactive, quiet, control.clone())?
        }
    };

    if let Some(c) = &control {
        c.shutdown();
    }
    println!("{report}");
    Ok(())
}
