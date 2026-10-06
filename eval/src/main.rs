//! `cargo eval`: runs genji's evaluation levels on Harbor and summarizes them like `cargo test`.
//!
//! `eval/catalog.toml` describes every task: hand-authored ones in `eval/tasks/` and registry
//! ones pinned in `eval/dataset.toml`, each with a level (smoke, unit, benchmark), capability,
//! difficulty and genji config overrides. Its `[benchmarks.*]` suites are whole registry
//! datasets (filtered by category and sampled) or catalog tasks of level benchmark. Every run
//! uses the runner's genji config (`--config`, default `eval/config.json`). Harbor runs
//! locally; `eval/remote.py` runs these commands on another host.

use anyhow::{Context, Result, bail};
use genji_eval::copy_dir;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "usage:
  cargo eval smoke   [filter...]                 does genji work at all
  cargo eval unit    [filter...]                 every promised feature, one small task each
  cargo eval bench   SUITE [filter...]           a standard benchmark suite
  cargo eval run     [filter...] [--level L | --suite S]
  cargo eval list    [filter...] [--level L | --suite S]
  cargo eval check   [filter...] [--level L]     hand-authored tasks: nop scores 0, oracle scores 1
  cargo eval report  [JOB|latest]
  cargo eval compare JOB_A JOB_B

options: --config FILE (default eval/config.json, see eval/config.example.json), -k ATTEMPTS
(1; `run`: 3), -j CONCURRENCY, --agents DIR, --capability C, --difficulty D, --name JOB.
A filter matches a task name or capability by substring. Levels: smoke, unit, benchmark.
`cargo eval list` without a level or suite also lists the benchmark suites.";

const LEVELS: [&str; 3] = ["smoke", "unit", "benchmark"];

#[derive(Default)]
struct Opts {
    cmd: String,
    positional: Vec<String>,
    attempts: Option<u32>,
    concurrency: u32,
    config: Option<String>,
    agents: Option<String>,
    level: Option<String>,
    suite: Option<String>,
    capability: Option<String>,
    difficulty: Option<String>,
    name: Option<String>,
}

fn parse(args: Vec<String>) -> Result<Opts> {
    let mut o = Opts {
        concurrency: 1,
        ..Default::default()
    };
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with('-') => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = || -> Result<String> {
            inline
                .clone()
                .or_else(|| it.next())
                .with_context(|| format!("{flag} needs a value"))
        };
        match flag.as_str() {
            "-k" | "--attempts" => {
                o.attempts = Some(value()?.parse().context("-k needs a number")?)
            }
            "-j" | "--concurrency" => {
                o.concurrency = value()?.parse().context("-j needs a number")?
            }
            "--config" => o.config = Some(value()?),
            "--agents" => o.agents = Some(value()?),
            "--level" => o.level = Some(value()?),
            "--suite" => o.suite = Some(value()?),
            "--capability" => o.capability = Some(value()?),
            "--difficulty" => o.difficulty = Some(value()?),
            "--name" => o.name = Some(value()?),
            "-h" | "--help" => o.cmd = "help".into(),
            f if f.starts_with('-') => bail!("unknown option `{f}`"),
            _ if o.cmd.is_empty() => o.cmd = arg,
            _ => o.positional.push(arg),
        }
    }
    // The level commands are `run` with the level set; `bench` takes the suite first.
    match o.cmd.as_str() {
        "smoke" | "unit" => {
            o.level = Some(std::mem::replace(&mut o.cmd, "run".into()));
        }
        "bench" => {
            anyhow::ensure!(
                !o.positional.is_empty(),
                "usage: cargo eval bench SUITE [filter...]"
            );
            o.suite = Some(o.positional.remove(0));
            o.cmd = "run".into();
        }
        "run" => {
            o.attempts.get_or_insert(3);
        }
        _ => {}
    }
    o.attempts.get_or_insert(1);
    if let Some(l) = &o.level {
        anyhow::ensure!(
            LEVELS.contains(&l.as_str()),
            "unknown level `{l}`; levels: {LEVELS:?}"
        );
    }
    anyhow::ensure!(
        o.level.is_none() || o.suite.is_none(),
        "--level and --suite are exclusive"
    );
    Ok(o)
}

fn main() {
    let code = parse(std::env::args().skip(1).collect()).and_then(|o| match o.cmd.as_str() {
        "list" => cmd_list(&o),
        "run" => cmd_run(&o),
        "check" => cmd_check(&o),
        "report" => cmd_report(&o),
        "compare" => cmd_compare(&o),
        _ => {
            println!("{USAGE}");
            Ok(if o.cmd == "help" { 0 } else { 2 })
        }
    });
    match code {
        Ok(c) => std::process::exit(c),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
    }
}

// -- catalog -------------------------------------------------------------- //

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn eval_dir() -> PathBuf {
    root().join("eval")
}

#[derive(Clone, Debug, Default)]
struct Task {
    name: String,
    level: String,
    /// The benchmark suite of a level-benchmark task.
    suite: String,
    capability: String,
    difficulty: String,
    /// The task directory relative to `eval/`: hand-authored (`tasks/...`) or from a
    /// downloaded benchmark dataset (`.store/...`).
    path: Option<String>,
    /// From the registry: the digest `dataset.toml` pins.
    digest: Option<String>,
}

impl Task {
    /// Hand-authored in `eval/tasks/`.
    fn local(&self) -> bool {
        self.path.as_ref().is_some_and(|p| p.starts_with("tasks/"))
    }

    /// Ships an oracle solution. A task whose verifier reads what genji did has none, since
    /// nothing short of a genji run satisfies it.
    fn has_oracle(&self) -> bool {
        self.path
            .as_ref()
            .is_some_and(|p| !find(&eval_dir().join(p), "solve.sh").is_empty())
    }
}

/// A `[benchmarks.NAME]` suite: a registry dataset, optionally filtered by
/// `[metadata].category` and sampled, or else the catalog tasks with `suite = NAME`.
#[derive(Clone, Debug, Default)]
struct Suite {
    name: String,
    description: String,
    /// `org/name@revision`.
    dataset: Option<String>,
    category: Vec<String>,
    sample: Option<usize>,
}

fn load_toml(path: &Path) -> Result<toml::Table> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    text.parse()
        .with_context(|| format!("parse {}", path.display()))
}

/// Registry task names in `dataset.toml` mapped to their pinned digests.
fn registry_tasks(dir: &Path) -> Result<BTreeMap<String, String>> {
    let ds = load_toml(&dir.join("dataset.toml"))?;
    let mut out = BTreeMap::new();
    for t in ds
        .get("tasks")
        .and_then(|t| t.as_array())
        .into_iter()
        .flatten()
    {
        let name = t["name"].as_str().context("dataset.toml: task name")?;
        let digest = t["digest"]
            .as_str()
            .with_context(|| format!("dataset.toml: {name} has no digest"))?;
        out.insert(name.to_string(), digest.to_string());
    }
    Ok(out)
}

/// The catalog's tasks, each found in `eval/tasks/` or pinned in `dataset.toml`.
fn catalog(dir: &Path) -> Result<Vec<Task>> {
    let cat = load_toml(&dir.join("catalog.toml"))?;
    let local = local_tasks(dir)?;
    let registry = registry_tasks(dir)?;
    let mut out = Vec::new();
    for (name, entry) in cat
        .get("tasks")
        .and_then(|t| t.as_table())
        .into_iter()
        .flatten()
    {
        let field = |k: &str| {
            entry
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        out.push(Task {
            name: name.clone(),
            level: field("level"),
            suite: field("suite"),
            capability: field("capability"),
            difficulty: field("difficulty"),
            path: local.get(name).map(|p| {
                p.strip_prefix(dir)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .into_owned()
            }),
            digest: registry.get(name).cloned(),
        });
    }
    Ok(out)
}

/// The catalog's `[benchmarks]` suites.
fn suites(dir: &Path) -> Result<Vec<Suite>> {
    let cat = load_toml(&dir.join("catalog.toml"))?;
    let mut out = Vec::new();
    for (name, s) in cat
        .get("benchmarks")
        .and_then(|t| t.as_table())
        .into_iter()
        .flatten()
    {
        let text = |k: &str| s.get(k).and_then(|v| v.as_str()).map(String::from);
        out.push(Suite {
            name: name.clone(),
            description: text("description").unwrap_or_default(),
            dataset: text("dataset"),
            category: s
                .get("category")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            sample: s
                .get("sample")
                .and_then(|v| v.as_integer())
                .map(|n| n as usize),
        });
    }
    Ok(out)
}

/// The tasks of a suite. A dataset is downloaded once into `eval/.store/`.
fn suite_tasks(dir: &Path, suite: &Suite) -> Result<Vec<Task>> {
    let Some(dataset) = &suite.dataset else {
        return Ok(catalog(dir)?
            .into_iter()
            .filter(|t| t.level == "benchmark" && t.suite == suite.name)
            .collect());
    };
    let rel = format!(".store/{}", dataset.replace('/', "__"));
    let store = dir.join(&rel);
    if !store.is_dir() {
        let tmp = dir.join(format!("{rel}.part"));
        let _ = fs::remove_dir_all(&tmp);
        sh(Command::new("harbor")
            .args(["download", dataset, "-o"])
            .arg(&tmp))?;
        fs::rename(&tmp, &store)?;
    }
    dataset_tasks(dir, &rel, suite)
}

/// Task directories under `eval/<rel>`, filtered by the suite's categories and sampled.
fn dataset_tasks(dir: &Path, rel: &str, suite: &Suite) -> Result<Vec<Task>> {
    let mut tasks = Vec::new();
    for task_toml in find(&dir.join(rel), "task.toml") {
        let t = load_toml(&task_toml)?;
        let meta = |k: &str| {
            t.get("metadata")
                .and_then(|m| m.get(k))
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string()
        };
        let category = meta("category");
        if !suite.category.is_empty() && !suite.category.contains(&category) {
            continue;
        }
        let task_dir = task_toml.parent().unwrap();
        let name = t
            .get("task")
            .and_then(|t| t.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| task_dir.file_name().unwrap().to_string_lossy().into_owned());
        tasks.push(Task {
            name,
            level: "benchmark".into(),
            suite: suite.name.clone(),
            capability: if category.is_empty() {
                suite.name.clone()
            } else {
                category
            },
            difficulty: meta("difficulty"),
            path: Some(
                task_dir
                    .strip_prefix(dir)
                    .unwrap_or(task_dir)
                    .to_string_lossy()
                    .into_owned(),
            ),
            digest: None,
        });
    }
    if let Some(n) = suite.sample {
        // A fixed pseudo-random sample: the same names across runs and hosts.
        tasks.sort_by_key(|t| fnv1a(&t.name));
        tasks.truncate(n);
    }
    tasks.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(tasks)
}

fn fnv1a(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

/// The tasks a command works on: a suite's, a level's, or every catalog task.
fn tasks(o: &Opts) -> Result<Vec<Task>> {
    let dir = eval_dir();
    let all = match (&o.suite, &o.level) {
        (Some(name), _) => {
            let all = suites(&dir)?;
            let suite = all.iter().find(|s| &s.name == name).with_context(|| {
                let names: Vec<&str> = all.iter().map(|s| s.name.as_str()).collect();
                format!("unknown suite `{name}`; suites: {names:?}")
            })?;
            suite_tasks(&dir, suite)?
        }
        (None, Some(level)) => catalog(&dir)?
            .into_iter()
            .filter(|t| &t.level == level)
            .collect(),
        (None, None) => catalog(&dir)?,
    };
    Ok(select(o, all))
}

/// Hand-authored task names mapped to their directories.
fn local_tasks(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.join("tasks")];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.join("task.toml").is_file() {
                let t = load_toml(&p.join("task.toml"))?;
                let name = t["task"]["name"].as_str().context("task.name")?.to_string();
                out.insert(name, p);
            } else if p.is_dir() {
                stack.push(p);
            }
        }
    }
    Ok(out)
}

fn select(o: &Opts, tasks: Vec<Task>) -> Vec<Task> {
    tasks
        .into_iter()
        .filter(|t| {
            o.positional.is_empty()
                || o.positional
                    .iter()
                    .any(|f| t.name.contains(f.as_str()) || t.capability.contains(f.as_str()))
        })
        .filter(|t| o.capability.as_ref().is_none_or(|c| &t.capability == c))
        .filter(|t| o.difficulty.as_ref().is_none_or(|d| &t.difficulty == d))
        .collect()
}

fn cmd_list(o: &Opts) -> Result<i32> {
    let tasks = tasks(o)?;
    for t in &tasks {
        let src = match (&t.path, t.local()) {
            (_, true) => "local",
            (Some(_), false) => "dataset",
            (None, _) => "registry",
        };
        let level = if t.suite.is_empty() {
            t.level.clone()
        } else {
            format!("{}/{}", t.level, t.suite)
        };
        println!(
            "{:<60} {:<22} {:<22} {:<7} {src}",
            t.name, level, t.capability, t.difficulty
        );
    }
    println!("{} task(s)", tasks.len());
    if o.level.is_none() && o.suite.is_none() {
        println!("\nbenchmark suites (cargo eval bench SUITE):");
        for s in suites(&eval_dir())? {
            println!("  {:<16} {}", s.name, s.description);
        }
    }
    Ok(0)
}

// -- running Harbor ------------------------------------------------------- //

fn sh(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("run {cmd:?}"))?;
    anyhow::ensure!(status.success(), "{cmd:?} failed: {status}");
    Ok(())
}

fn capture(cmd: &mut Command) -> String {
    cmd.output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default()
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Runs a shell script in the repository, with the Harbor adapter importable.
fn in_repo(script: &str) -> Result<()> {
    sh(Command::new("bash")
        .args(["-c", script])
        .current_dir(root())
        .env("PYTHONPATH", eval_dir().join("harbor")))
}

/// Builds static genji and genji-drive, which Harbor uploads into task containers.
fn build() -> Result<()> {
    in_repo(
        "RUSTFLAGS='-C target-feature=+crt-static' cargo build -q --release \
         --target $(uname -m)-unknown-linux-gnu --workspace --bins",
    )
}

fn timestamp() -> String {
    capture(Command::new("date").arg("+%Y%m%d-%H%M%S"))
}

/// A script that lays out the job's tasks as a Harbor local dataset, `eval/.runs/<job>/`:
/// symlinks to hand-authored tasks and to registry tasks downloaded once per digest into
/// `eval/.store/`. Harbor resolves a `dataset.toml` of digests only for published datasets.
fn materialize(job: &str, tasks: &[Task]) -> String {
    let mut s = format!("set -e; mkdir -p eval/.store eval/.runs/{job}; cd eval/.runs/{job}\n");
    for t in tasks {
        let link = quote(&t.name.replace('/', "__"));
        match (&t.path, &t.digest) {
            (Some(path), _) => {
                let _ = writeln!(s, "ln -sfn {} {link}", quote(&format!("../../{path}")));
            }
            (None, Some(digest)) => {
                let hex = digest.trim_start_matches("sha256:");
                let short = t.name.rsplit('/').next().unwrap_or(&t.name);
                let _ = writeln!(
                    s,
                    "[ -d ../../.store/{hex}/{short} ] || harbor download {} -o ../../.store/{hex} >/dev/null\n\
                     ln -sfn {} {link}",
                    quote(&format!("{}@{digest}", t.name)),
                    quote(&format!("../../.store/{hex}/{short}")),
                );
            }
            (None, None) => s.push_str(&format!("echo 'unknown task {}' >&2; exit 1\n", t.name)),
        }
    }
    s
}

fn harbor_run(o: &Opts, job: &str, agent: &str, tasks: &[Task], attempts: u32) -> Result<()> {
    in_repo(&format!("( {} )", materialize(job, tasks)))?;
    let mut args = vec![
        format!("harbor run -y -q -p eval/.runs/{job} -o eval/jobs"),
        format!(
            "--job-name {job} -a {agent} -k {attempts} -n {}",
            o.concurrency
        ),
    ];
    if agent.starts_with("genji_agent") {
        args.push("--artifact /app --ak catalog=eval/catalog.toml".into());
        let config = config_path(o)?;
        args.push(format!(
            "--ak genji_config={}",
            quote(&config.to_string_lossy())
        ));
        if let Some(a) = &o.agents {
            // Kept with the job.
            copy_dir(
                Path::new(a),
                &eval_dir().join("jobs").join(job).join("agents"),
            )?;
            args.push(format!("--ak agents_dir=eval/jobs/{job}/agents"));
        }
    }
    in_repo(&args.join(" "))
}

/// The runner's genji config: `--config`, else `eval/config.json`.
fn config_path(o: &Opts) -> Result<PathBuf> {
    let path = match &o.config {
        Some(c) => fs::canonicalize(c).with_context(|| format!("--config {c}"))?,
        None => eval_dir().join("config.json"),
    };
    anyhow::ensure!(
        path.is_file(),
        "no genji config at {}; copy eval/config.example.json to eval/config.json and fill it in, or pass --config FILE",
        path.display()
    );
    Ok(path)
}

/// The config with its secrets removed, for the job's provenance.
fn redacted(mut cfg: Value) -> Value {
    if let Some(providers) = cfg["providers"].as_object_mut() {
        for p in providers.values_mut() {
            if let Some(p) = p.as_object_mut() {
                if p.contains_key("api_key") {
                    p.insert("api_key".into(), json!("<redacted>"));
                }
                p.remove("headers");
            }
        }
    }
    cfg
}

fn provenance(o: &Opts, job: &str, tasks: &[Task]) -> Result<()> {
    let dir = eval_dir().join("jobs").join(job);
    fs::create_dir_all(&dir)?;
    let git = |args: &[&str]| capture(Command::new("git").args(args).current_dir(root()));
    let dirty = !git(&["status", "--porcelain", "--untracked-files=no"]).is_empty();
    if dirty {
        fs::write(dir.join("genji.diff"), git(&["diff", "HEAD"]))?;
    }
    let config = fs::read_to_string(config_path(o)?)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(redacted);
    let info = json!({
        "commit": git(&["rev-parse", "HEAD"]),
        "dirty": dirty,
        "host": capture(&mut Command::new("hostname")),
        "config": config,
        "level": o.level,
        "suite": o.suite,
        "agents": o.agents,
        "attempts": o.attempts,
        "filters": o.positional,
        "capability": o.capability,
        "difficulty": o.difficulty,
    });
    fs::write(dir.join("genji.json"), serde_json::to_string_pretty(&info)?)?;
    write_tasks(&dir, tasks)
}

/// What `report` needs to know about the job's tasks, kept with the job.
fn write_tasks(job: &Path, tasks: &[Task]) -> Result<()> {
    let json: Vec<Value> = tasks
        .iter()
        .map(|t| {
            json!({"name": t.name, "level": t.level, "suite": t.suite,
                        "capability": t.capability, "difficulty": t.difficulty})
        })
        .collect();
    fs::write(job.join("tasks.json"), serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

fn cmd_run(o: &Opts) -> Result<i32> {
    let tasks = tasks(o)?;
    anyhow::ensure!(!tasks.is_empty(), "no task matches");
    let prefix = o.suite.as_deref().or(o.level.as_deref()).unwrap_or("eval");
    let job = o
        .name
        .clone()
        .unwrap_or_else(|| format!("{prefix}-{}", timestamp()));
    provenance(o, &job, &tasks)?;
    build()?;
    harbor_run(
        o,
        &job,
        "genji_agent:Genji",
        &tasks,
        o.attempts.unwrap_or(1),
    )?;
    report(&eval_dir().join("jobs").join(&job))
}

fn cmd_check(o: &Opts) -> Result<i32> {
    let tasks: Vec<Task> = tasks(o)?.into_iter().filter(Task::local).collect();
    anyhow::ensure!(!tasks.is_empty(), "no hand-authored task matches");
    let stamp = timestamp();
    let mut bad = 0;
    for (agent, want) in [("nop", 0.0), ("oracle", 1.0)] {
        let tasks: Vec<Task> = tasks
            .iter()
            .filter(|t| agent == "nop" || t.has_oracle())
            .cloned()
            .collect();
        if tasks.is_empty() {
            continue;
        }
        let job = format!("check-{agent}-{stamp}");
        harbor_run(o, &job, agent, &tasks, 1)?;
        for (task, trials) in trials_by_task(&eval_dir().join("jobs").join(&job))? {
            for t in trials {
                if t.reward != Some(want) {
                    bad += 1;
                    println!(
                        "check {task}: {agent} scored {:?}, expected {want}",
                        t.reward
                    );
                }
            }
        }
    }
    println!("check: {} task(s), {bad} problem(s)", tasks.len());
    Ok(if bad == 0 { 0 } else { 1 })
}

// -- results -------------------------------------------------------------- //

#[derive(Debug, Default, Clone)]
struct Trial {
    reward: Option<f64>,
    error: Option<String>,
    tokens: u64,
    secs: f64,
}

/// Every trial of a job, grouped by task name. A trial's tokens and time come from the
/// metrics.json files genji-drive wrote for its steps.
fn trials_by_task(job: &Path) -> Result<BTreeMap<String, Vec<Trial>>> {
    let mut out: BTreeMap<String, Vec<Trial>> = BTreeMap::new();
    let entries = fs::read_dir(job).with_context(|| format!("read {}", job.display()))?;
    for e in entries.flatten() {
        let path = e.path().join("result.json");
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let Ok(r) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(task) = r["task_name"].as_str() else {
            continue;
        };
        let mut t = Trial {
            reward: r["verifier_result"]["rewards"]["reward"].as_f64(),
            error: r["exception_info"]["exception_type"]
                .as_str()
                .map(String::from),
            ..Default::default()
        };
        for m in find(&e.path(), "metrics.json") {
            let Ok(v) = fs::read_to_string(&m)
                .map(|s| serde_json::from_str::<Value>(&s).unwrap_or_default())
            else {
                continue;
            };
            t.tokens += v["prompt_tokens"].as_u64().unwrap_or(0)
                + v["completion_tokens"].as_u64().unwrap_or(0);
            t.secs += v["duration_secs"].as_f64().unwrap_or(0.0);
        }
        out.entry(task.to_string()).or_default().push(t);
    }
    Ok(out)
}

fn find(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            out.extend(find(&p, name));
        } else if p.file_name().is_some_and(|n| n == name) {
            out.push(p);
        }
    }
    out
}

fn passed(t: &Trial) -> bool {
    t.reward.is_some_and(|r| r >= 1.0)
}

struct Row {
    task: String,
    capability: String,
    difficulty: String,
    passed: usize,
    total: usize,
    tokens: u64,
    secs: f64,
    errors: Vec<String>,
}

impl Row {
    fn verdict(&self) -> &'static str {
        match self.passed {
            p if p == self.total => "ok",
            0 => "FAILED",
            _ => "flaky",
        }
    }
}

/// The job's tasks (from its tasks.json, else the catalog) by name.
fn job_tasks(job: &Path) -> BTreeMap<String, Task> {
    let mut out: BTreeMap<String, Task> = catalog(&eval_dir())
        .unwrap_or_default()
        .into_iter()
        .map(|t| (t.name.clone(), t))
        .collect();
    let saved: Vec<Value> = fs::read_to_string(job.join("tasks.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    for t in saved {
        let f = |k: &str| t[k].as_str().unwrap_or_default().to_string();
        out.insert(
            f("name"),
            Task {
                name: f("name"),
                level: f("level"),
                suite: f("suite"),
                capability: f("capability"),
                difficulty: f("difficulty"),
                ..Default::default()
            },
        );
    }
    out
}

fn rows(job: &Path) -> Result<Vec<Row>> {
    let cat = job_tasks(job);
    Ok(trials_by_task(job)?
        .into_iter()
        .map(|(task, trials)| {
            let n = trials.len().max(1) as f64;
            let c = cat.get(&task);
            Row {
                capability: c
                    .map(|c| c.capability.clone())
                    .unwrap_or_else(|| "-".into()),
                difficulty: c
                    .map(|c| c.difficulty.clone())
                    .unwrap_or_else(|| "-".into()),
                passed: trials.iter().filter(|t| passed(t)).count(),
                total: trials.len(),
                tokens: (trials.iter().map(|t| t.tokens).sum::<u64>() as f64 / n) as u64,
                secs: trials.iter().map(|t| t.secs).sum::<f64>() / n,
                errors: trials.iter().filter_map(|t| t.error.clone()).collect(),
                task,
            }
        })
        .collect())
}

fn human_tokens(n: u64) -> String {
    match n {
        n if n >= 1_000_000 => format!("{:.1}M", n as f64 / 1e6),
        n if n >= 1_000 => format!("{}k", n / 1000),
        n => n.to_string(),
    }
}

fn human_secs(s: f64) -> String {
    let s = s as u64;
    if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

/// Prints the job like `cargo test`, writes summary.md/summary.json into it, and fails
/// when a task passed no trial.
fn report(job: &Path) -> Result<i32> {
    let rows = rows(job)?;
    let mut md = String::from(
        "| task | capability | difficulty | passed | avg tokens | avg time | errors |\n|---|---|---|---|---|---|---|\n",
    );
    let mut by_cap: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for r in &rows {
        let errors = if r.errors.is_empty() {
            String::new()
        } else {
            format!(" [{}]", r.errors.join(", "))
        };
        println!(
            "test {} ... {} {}/{} ({} tok, {}){errors}",
            r.task,
            r.verdict(),
            r.passed,
            r.total,
            human_tokens(r.tokens),
            human_secs(r.secs)
        );
        let _ = writeln!(
            md,
            "| {} | {} | {} | {}/{} | {} | {} | {} |",
            r.task,
            r.capability,
            r.difficulty,
            r.passed,
            r.total,
            human_tokens(r.tokens),
            human_secs(r.secs),
            r.errors.join(", ")
        );
        let e = by_cap.entry(r.capability.clone()).or_default();
        e.0 += r.passed;
        e.1 += r.total;
    }
    let pass: usize = rows.iter().map(|r| r.passed).sum();
    let total: usize = rows.iter().map(|r| r.total).sum();
    let failed = rows.iter().filter(|r| r.verdict() == "FAILED").count();
    let flaky = rows.iter().filter(|r| r.verdict() == "flaky").count();
    println!();
    md.push_str("\n| capability | pass rate |\n|---|---|\n");
    for (cap, (p, t)) in &by_cap {
        println!(
            "{cap:<14} {p}/{t} ({:.0}%)",
            100.0 * *p as f64 / (*t).max(1) as f64
        );
        let _ = writeln!(md, "| {cap} | {p}/{t} |");
    }
    println!(
        "\ntest result: {}. {} ok; {flaky} flaky; {failed} failed; trials {pass}/{total} passed; job {}",
        if failed == 0 { "ok" } else { "FAILED" },
        rows.len() - flaky - failed,
        job.display()
    );
    fs::write(job.join("summary.md"), md)?;
    let json: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({"task": r.task, "capability": r.capability, "difficulty": r.difficulty,
                   "passed": r.passed, "total": r.total, "avg_tokens": r.tokens,
                   "avg_secs": r.secs, "errors": r.errors, "verdict": r.verdict()})
        })
        .collect();
    fs::write(
        job.join("summary.json"),
        serde_json::to_string_pretty(&json)?,
    )?;
    Ok(if failed == 0 { 0 } else { 1 })
}

fn job_dir(name: Option<&String>) -> Result<PathBuf> {
    let jobs = eval_dir().join("jobs");
    match name.map(String::as_str) {
        Some(n) if n != "latest" => Ok(if Path::new(n).is_dir() {
            PathBuf::from(n)
        } else {
            jobs.join(n)
        }),
        _ => {
            let mut dirs: Vec<PathBuf> = fs::read_dir(&jobs)
                .with_context(|| format!("read {}", jobs.display()))?
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.join("genji.json").is_file())
                .collect();
            dirs.sort_by_key(|p| fs::metadata(p).and_then(|m| m.modified()).ok());
            dirs.pop().context("no eval job yet")
        }
    }
}

fn cmd_report(o: &Opts) -> Result<i32> {
    report(&job_dir(o.positional.first())?)
}

fn cmd_compare(o: &Opts) -> Result<i32> {
    let [a, b] = o.positional.as_slice() else {
        bail!("usage: cargo eval compare JOB_A JOB_B")
    };
    let ra: BTreeMap<String, Row> = rows(&job_dir(Some(a))?)?
        .into_iter()
        .map(|r| (r.task.clone(), r))
        .collect();
    let rb: BTreeMap<String, Row> = rows(&job_dir(Some(b))?)?
        .into_iter()
        .map(|r| (r.task.clone(), r))
        .collect();
    let rate = |r: &Row| r.passed as f64 / r.total.max(1) as f64;
    println!(
        "{:<60} {:>9} {:>9} {:>10} {:>10}",
        "task", "a", "b", "a tok", "b tok"
    );
    let names: std::collections::BTreeSet<&String> = ra.keys().chain(rb.keys()).collect();
    for name in names {
        let (x, y) = (ra.get(name), rb.get(name));
        let cell = |r: Option<&Row>| {
            r.map(|r| format!("{}/{}", r.passed, r.total))
                .unwrap_or("-".into())
        };
        let tok = |r: Option<&Row>| r.map(|r| human_tokens(r.tokens)).unwrap_or("-".into());
        let mark = match (x, y) {
            (Some(x), Some(y)) if rate(y) > rate(x) => " +",
            (Some(x), Some(y)) if rate(y) < rate(x) => " -",
            _ => "",
        };
        println!(
            "{name:<60} {:>9} {:>9} {:>10} {:>10}{mark}",
            cell(x),
            cell(y),
            tok(x),
            tok(y)
        );
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The catalog describes exactly the hand-authored and the pinned registry tasks.
    #[test]
    fn catalog_matches_tasks() {
        let dir = eval_dir();
        let cat: Vec<String> = catalog(&dir).unwrap().into_iter().map(|t| t.name).collect();
        let local = local_tasks(&dir).unwrap();
        let registry = registry_tasks(&dir).unwrap();
        let mut known: Vec<String> = local.keys().chain(registry.keys()).cloned().collect();
        known.sort();
        assert_eq!(
            cat, known,
            "eval/catalog.toml does not match eval/tasks/ + eval/dataset.toml"
        );
        for name in local.keys() {
            assert!(
                !registry.contains_key(name),
                "{name} is both hand-authored and in dataset.toml"
            );
        }
    }

    #[test]
    fn materialize_links_local_and_pinned_tasks() {
        let tasks = [
            Task {
                name: "genji/x".into(),
                path: Some("tasks/a/x".into()),
                ..Default::default()
            },
            Task {
                name: "org/y".into(),
                digest: Some("sha256:ab".into()),
                ..Default::default()
            },
        ];
        let s = materialize("j", &tasks);
        assert!(s.contains("ln -sfn '../../tasks/a/x' 'genji__x'"));
        assert!(s.contains("harbor download 'org/y@sha256:ab' -o ../../.store/ab"));
        assert!(s.contains("ln -sfn '../../.store/ab/y' 'org__y'"));
    }

    #[test]
    fn catalog_entries_are_complete() {
        let suites = suites(&eval_dir()).unwrap();
        for t in catalog(&eval_dir()).unwrap() {
            assert!(!t.capability.is_empty(), "{}: no capability", t.name);
            assert!(
                LEVELS.contains(&t.level.as_str()),
                "{}: level {:?}",
                t.name,
                t.level
            );
            assert_eq!(
                t.level == "benchmark",
                suites
                    .iter()
                    .any(|s| s.dataset.is_none() && s.name == t.suite),
                "{}: a benchmark-level task, and only one, names a catalog suite",
                t.name
            );
            assert!(
                ["easy", "medium"].contains(&t.difficulty.as_str()),
                "{}: difficulty {:?}",
                t.name,
                t.difficulty
            );
        }
    }

    /// Every step of a hand-authored task has a prompt and a verifier, and its front matter
    /// parses.
    #[test]
    fn local_tasks_are_well_formed() {
        for (name, dir) in local_tasks(&eval_dir()).unwrap() {
            let t = load_toml(&dir.join("task.toml")).unwrap();
            let steps: Vec<PathBuf> = match t.get("steps").and_then(|s| s.as_array()) {
                Some(steps) => steps
                    .iter()
                    .map(|s| dir.join("steps").join(s["name"].as_str().unwrap()))
                    .collect(),
                None => vec![dir.clone()],
            };
            for s in steps {
                for f in ["instruction.md", "tests/test.sh", "tests/test.py"] {
                    assert!(
                        s.join(f).is_file(),
                        "{name}: missing {}",
                        s.join(f).display()
                    );
                }
                let text = fs::read_to_string(s.join("instruction.md")).unwrap();
                if let Some(rest) = text.strip_prefix("+++\n") {
                    let (head, _) = rest
                        .split_once("\n+++\n")
                        .unwrap_or_else(|| panic!("{name}: unclosed front matter"));
                    head.parse::<toml::Table>()
                        .unwrap_or_else(|e| panic!("{name}: front matter: {e}"));
                }
            }
        }
    }

    /// Unit tasks stay quick: a slow one is a capability benchmark, not a feature check.
    #[test]
    fn unit_tasks_are_quick() {
        let local = local_tasks(&eval_dir()).unwrap();
        for t in catalog(&eval_dir())
            .unwrap()
            .iter()
            .filter(|t| t.level == "unit")
        {
            let dir = local
                .get(&t.name)
                .unwrap_or_else(|| panic!("{}: not hand-authored", t.name));
            let toml = load_toml(&dir.join("task.toml")).unwrap();
            let secs = |k: &str| toml[k]["timeout_sec"].as_float().unwrap_or(f64::MAX);
            assert!(
                secs("agent") <= 120.0,
                "{}: agent timeout over 120s",
                t.name
            );
            assert!(
                secs("verifier") <= 30.0,
                "{}: verifier timeout over 30s",
                t.name
            );
        }
    }

    #[test]
    fn suites_are_well_formed() {
        for s in suites(&eval_dir()).unwrap() {
            assert!(!s.description.is_empty(), "{}: no description", s.name);
            if let Some(d) = &s.dataset {
                assert!(
                    d.contains('/') && d.contains('@'),
                    "{}: pin `org/name@revision`",
                    s.name
                );
            } else {
                assert!(
                    !suite_tasks(&eval_dir(), &s).unwrap().is_empty(),
                    "{}: no task",
                    s.name
                );
            }
        }
    }

    #[test]
    fn dataset_suites_filter_by_category_and_sample_stably() {
        let dir = std::env::temp_dir().join(format!("genji-eval-ds-{}", std::process::id()));
        for (n, cat) in [
            ("a", "debugging"),
            ("b", "games"),
            ("c", "debugging"),
            ("d", "debugging"),
        ] {
            let t = dir.join(".store/ds/x").join(n);
            fs::create_dir_all(&t).unwrap();
            let toml = format!(
                "[task]\nname = \"org/{n}\"\n[metadata]\ncategory = \"{cat}\"\ndifficulty = \"hard\"\n"
            );
            fs::write(t.join("task.toml"), toml).unwrap();
        }
        let suite = |category: &[&str], sample| Suite {
            name: "s".into(),
            category: category.iter().map(|c| c.to_string()).collect(),
            sample,
            ..Default::default()
        };
        let names = |s: &Suite| -> Vec<String> {
            dataset_tasks(&dir, ".store/ds", s)
                .unwrap()
                .into_iter()
                .map(|t| t.name)
                .collect()
        };
        assert_eq!(names(&suite(&[], None)).len(), 4);
        assert_eq!(
            names(&suite(&["debugging"], None)),
            ["org/a", "org/c", "org/d"]
        );
        let sampled = names(&suite(&["debugging"], Some(2)));
        assert_eq!(sampled.len(), 2);
        assert_eq!(sampled, names(&suite(&["debugging"], Some(2))));
        let t = &dataset_tasks(&dir, ".store/ds", &suite(&["games"], None)).unwrap()[0];
        assert_eq!(
            (t.capability.as_str(), t.difficulty.as_str()),
            ("games", "hard")
        );
        assert_eq!(t.path.as_deref(), Some(".store/ds/x/b"));
        assert!(!t.local());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn level_commands_set_the_level_or_suite() {
        let p = |args: &[&str]| parse(args.iter().map(|s| s.to_string()).collect()).unwrap();
        let o = p(&["unit", "socket"]);
        assert_eq!(
            (o.cmd.as_str(), o.level.as_deref(), o.attempts),
            ("run", Some("unit"), Some(1))
        );
        assert_eq!(o.positional, ["socket"]);
        let o = p(&["bench", "tb-light", "-k", "2"]);
        assert_eq!(
            (o.suite.as_deref(), o.attempts),
            (Some("tb-light"), Some(2))
        );
        assert_eq!(p(&["run"]).attempts, Some(3));
        assert!(parse(vec!["run".into(), "--level".into(), "nope".into()]).is_err());
        assert!(parse(vec!["bench".into()]).is_err());
    }

    #[test]
    fn provenance_redacts_keys() {
        let cfg = json!({"providers": {"p": {"api_key": "sk-secret", "headers": {"x": "y"}, "model": "m"}}});
        let r = redacted(cfg).to_string();
        assert!(!r.contains("sk-secret") && !r.contains("headers") && r.contains("\"m\""));
    }

    /// Every task's tests/verify.py and solution/oracle.py are copies of the ones in eval/lib.
    /// A verifier that does not parse would only show up inside a task container.
    #[test]
    fn verifiers_are_valid_python() {
        for (name, dir) in local_tasks(&eval_dir()).unwrap() {
            for test in find(&dir, "test.py") {
                let ok = Command::new("python3")
                    .args(["-c", "import ast, sys; ast.parse(open(sys.argv[1]).read())"])
                    .arg(&test)
                    .status()
                    .unwrap()
                    .success();
                assert!(ok, "{name}: {} does not parse", test.display());
            }
        }
    }

    #[test]
    fn report_summarizes_a_job() {
        let job = std::env::temp_dir().join(format!("genji-eval-test-{}", std::process::id()));
        let trial = |n: &str, task: &str, reward: Option<f64>, tokens: u64| {
            let d = job.join(n);
            fs::create_dir_all(d.join("agent")).unwrap();
            let r = json!({"task_name": task, "verifier_result": reward.map(|r| json!({"rewards": {"reward": r}})),
                           "exception_info": null});
            fs::write(d.join("result.json"), r.to_string()).unwrap();
            let m = json!({"prompt_tokens": tokens, "completion_tokens": 0, "duration_secs": 10.0});
            fs::write(d.join("agent/metrics.json"), m.to_string()).unwrap();
        };
        trial("a1", "x/ok", Some(1.0), 1000);
        trial("a2", "x/ok", Some(1.0), 3000);
        trial("b1", "x/flaky", Some(1.0), 0);
        trial("b2", "x/flaky", Some(0.0), 0);
        trial("c1", "x/fail", None, 0);
        let rows = rows(&job).unwrap();
        let get = |n: &str| rows.iter().find(|r| r.task == n).unwrap();
        assert_eq!((get("x/ok").verdict(), get("x/ok").tokens), ("ok", 2000));
        assert_eq!(get("x/flaky").verdict(), "flaky");
        assert_eq!(get("x/fail").verdict(), "FAILED");
        assert_eq!(report(&job).unwrap(), 1);
        assert!(job.join("summary.md").is_file());
        fs::remove_dir_all(&job).unwrap();
    }

    #[test]
    fn filters_select_by_name_capability_and_difficulty() {
        let t = |n: &str, c: &str, d: &str| Task {
            name: n.into(),
            capability: c.into(),
            difficulty: d.into(),
            ..Default::default()
        };
        let all = vec![
            t("genji/needle", "long-output", "easy"),
            t("quixbugs/python-gcd", "instruction", "medium"),
        ];
        let o = Opts {
            positional: vec!["long".into()],
            ..Default::default()
        };
        assert_eq!(select(&o, all.clone()).len(), 1);
        let o = Opts {
            difficulty: Some("medium".into()),
            ..Default::default()
        };
        assert_eq!(select(&o, all)[0].name, "quixbugs/python-gcd");
    }
}
