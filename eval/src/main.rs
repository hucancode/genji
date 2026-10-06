//! `cargo eval`: runs genji's evaluation sets on Harbor and summarizes them like `cargo test`.
//!
//! The easy set is `eval/dataset.toml` (pinned registry tasks plus `eval/tasks/`), described by
//! `eval/catalog.toml` (capability, difficulty, genji config overrides). The hard set is
//! Terminal-Bench in `benchmark/`. Harbor runs locally; `eval/remote.py` runs these commands on
//! another host.

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const USAGE: &str = "usage:
  cargo eval list    [filter...] [--capability C] [--difficulty D]
  cargo eval run     [filter...] [-k 3] [-j 1] [--profile haiku] [--model PROVIDER/MODEL]
                     [--agents DIR] [--capability C] [--difficulty D] [--name JOB]
  cargo eval check   [filter...]                   hand-authored tasks: nop scores 0, oracle scores 1
  cargo eval report  [JOB|latest]
  cargo eval compare JOB_A JOB_B
  cargo eval tb      [hello|light|full]            the hard set (Terminal-Bench 2.1)

A filter matches a task name or capability by substring. Profiles are eval/profiles/NAME.json; the easy set
runs on a weak model (haiku) unless --profile or --model says otherwise.";

/// Weak models expose agent weaknesses that strong ones paper over.
const DEFAULT_PROFILE: &str = "haiku";

#[derive(Default)]
struct Opts {
    cmd: String,
    positional: Vec<String>,
    attempts: u32,
    concurrency: u32,
    profile: Option<String>,
    model: Option<String>,
    agents: Option<String>,
    capability: Option<String>,
    difficulty: Option<String>,
    name: Option<String>,
}

fn parse(args: Vec<String>) -> Result<Opts> {
    let mut o = Opts {
        attempts: 3,
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
            "-k" | "--attempts" => o.attempts = value()?.parse().context("-k needs a number")?,
            "-j" | "--concurrency" => o.concurrency = value()?.parse().context("-j needs a number")?,
            "--profile" => o.profile = Some(value()?),
            "-m" | "--model" => o.model = Some(value()?),
            "--agents" => o.agents = Some(value()?),
            "--capability" => o.capability = Some(value()?),
            "--difficulty" => o.difficulty = Some(value()?),
            "--name" => o.name = Some(value()?),
            "-h" | "--help" => o.cmd = "help".into(),
            f if f.starts_with('-') => bail!("unknown option `{f}`"),
            _ if o.cmd.is_empty() => o.cmd = arg,
            _ => o.positional.push(arg),
        }
    }
    if o.cmd == "run" && o.profile.is_none() && o.model.is_none() {
        o.profile = Some(DEFAULT_PROFILE.into());
    }
    Ok(o)
}

fn main() {
    let code = parse(std::env::args().skip(1).collect()).and_then(|o| match o.cmd.as_str() {
        "list" => cmd_list(&o),
        "run" => cmd_run(&o),
        "check" => cmd_check(&o),
        "report" => cmd_report(&o),
        "compare" => cmd_compare(&o),
        "tb" => cmd_tb(&o),
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
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn eval_dir() -> PathBuf {
    root().join("eval")
}

#[derive(Clone, Debug, Default)]
struct Task {
    name: String,
    capability: String,
    difficulty: String,
    /// Hand-authored: the task directory, relative to `eval/`.
    path: Option<String>,
    /// From the registry: the digest `dataset.toml` pins.
    digest: Option<String>,
}

impl Task {
    fn local(&self) -> bool {
        self.path.is_some()
    }
}

fn load_toml(path: &Path) -> Result<toml::Table> {
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    text.parse().with_context(|| format!("parse {}", path.display()))
}

/// Registry task names in `dataset.toml` mapped to their pinned digests.
fn registry_tasks(dir: &Path) -> Result<BTreeMap<String, String>> {
    let ds = load_toml(&dir.join("dataset.toml"))?;
    let mut out = BTreeMap::new();
    for t in ds.get("tasks").and_then(|t| t.as_array()).into_iter().flatten() {
        let name = t["name"].as_str().context("dataset.toml: task name")?;
        let digest = t["digest"].as_str().with_context(|| format!("dataset.toml: {name} has no digest"))?;
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
    for (name, entry) in cat.get("tasks").and_then(|t| t.as_table()).into_iter().flatten() {
        let field = |k: &str| entry.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_string();
        out.push(Task {
            name: name.clone(),
            capability: field("capability"),
            difficulty: field("difficulty"),
            path: local
                .get(name)
                .map(|p| p.strip_prefix(dir).unwrap_or(p).to_string_lossy().into_owned()),
            digest: registry.get(name).cloned(),
        });
    }
    Ok(out)
}

/// Hand-authored task names mapped to their directories.
fn local_tasks(dir: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.join("tasks")];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else { continue };
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
                || o.positional.iter().any(|f| t.name.contains(f.as_str()) || t.capability.contains(f.as_str()))
        })
        .filter(|t| o.capability.as_ref().is_none_or(|c| &t.capability == c))
        .filter(|t| o.difficulty.as_ref().is_none_or(|d| &t.difficulty == d))
        .collect()
}

fn cmd_list(o: &Opts) -> Result<i32> {
    let tasks = select(o, catalog(&eval_dir())?);
    for t in &tasks {
        let src = if t.local() { "local" } else { "registry" };
        println!("{:<60} {:<13} {:<7} {src}", t.name, t.capability, t.difficulty);
    }
    println!("{} task(s)", tasks.len());
    Ok(0)
}

// -- running Harbor ------------------------------------------------------- //

fn sh(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("run {cmd:?}"))?;
    anyhow::ensure!(status.success(), "{cmd:?} failed: {status}");
    Ok(())
}

fn capture(cmd: &mut Command) -> String {
    cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Runs a shell script in the repository, with the Harbor adapter importable.
fn in_repo(script: &str) -> Result<()> {
    sh(Command::new("bash")
        .args(["-c", script])
        .current_dir(root())
        .env("PYTHONPATH", root().join("benchmark")))
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
        format!("--job-name {job} -a {agent} -k {attempts} -n {}", o.concurrency),
    ];
    if agent.starts_with("genji_agent") {
        args.push("--artifact /app --ak catalog=eval/catalog.toml".into());
        if let Some(p) = &o.profile {
            args.push(format!("--ak profile=eval/profiles/{p}.json"));
        }
        if let Some(m) = &o.model {
            args.push(format!("-m {}", quote(m)));
        }
        if let Some(a) = &o.agents {
            // Kept with the job.
            copy_dir(Path::new(a), &eval_dir().join("jobs").join(job).join("agents"))?;
            args.push(format!("--ak agents_dir=eval/jobs/{job}/agents"));
        }
    }
    in_repo(&args.join(" "))
}

fn provenance(o: &Opts, job: &str) -> Result<()> {
    let dir = eval_dir().join("jobs").join(job);
    fs::create_dir_all(&dir)?;
    let git = |args: &[&str]| capture(Command::new("git").args(args).current_dir(root()));
    let dirty = !git(&["status", "--porcelain", "--untracked-files=no"]).is_empty();
    if dirty {
        fs::write(dir.join("genji.diff"), git(&["diff", "HEAD"]))?;
    }
    let profile = o
        .profile
        .as_ref()
        .and_then(|p| fs::read_to_string(eval_dir().join("profiles").join(format!("{p}.json"))).ok())
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let info = json!({
        "commit": git(&["rev-parse", "HEAD"]),
        "dirty": dirty,
        "host": capture(&mut Command::new("hostname")),
        "profile": o.profile,
        "profile_config": profile,
        "model": o.model,
        "agents": o.agents,
        "attempts": o.attempts,
        "filters": o.positional,
        "capability": o.capability,
        "difficulty": o.difficulty,
    });
    fs::write(dir.join("genji.json"), serde_json::to_string_pretty(&info)?)?;
    Ok(())
}

fn cmd_run(o: &Opts) -> Result<i32> {
    let tasks = select(o, catalog(&eval_dir())?);
    anyhow::ensure!(!tasks.is_empty(), "no task matches");
    let job = o.name.clone().unwrap_or_else(|| format!("eval-{}", timestamp()));
    provenance(o, &job)?;
    build()?;
    harbor_run(o, &job, "genji_agent:Genji", &tasks, o.attempts)?;
    report(&eval_dir().join("jobs").join(&job))
}

fn cmd_check(o: &Opts) -> Result<i32> {
    let tasks: Vec<Task> = select(o, catalog(&eval_dir())?).into_iter().filter(Task::local).collect();
    anyhow::ensure!(!tasks.is_empty(), "no hand-authored task matches");
    let stamp = timestamp();
    let mut bad = 0;
    for (agent, want) in [("nop", 0.0), ("oracle", 1.0)] {
        let job = format!("check-{agent}-{stamp}");
        harbor_run(o, &job, agent, &tasks, 1)?;
        for (task, trials) in trials_by_task(&eval_dir().join("jobs").join(&job))? {
            for t in trials {
                if t.reward != Some(want) {
                    bad += 1;
                    println!("check {task}: {agent} scored {:?}, expected {want}", t.reward);
                }
            }
        }
    }
    println!("check: {} task(s), {bad} problem(s)", tasks.len());
    Ok(if bad == 0 { 0 } else { 1 })
}

fn cmd_tb(o: &Opts) -> Result<i32> {
    let target = o.positional.first().map(String::as_str).unwrap_or("light");
    in_repo(&format!("make -C benchmark {}", quote(target)))?;
    Ok(0)
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
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Ok(r) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(task) = r["task_name"].as_str() else { continue };
        let mut t = Trial {
            reward: r["verifier_result"]["rewards"]["reward"].as_f64(),
            error: r["exception_info"]["exception_type"].as_str().map(String::from),
            ..Default::default()
        };
        for m in find(&e.path(), "metrics.json") {
            let Ok(v) = fs::read_to_string(&m).map(|s| serde_json::from_str::<Value>(&s).unwrap_or_default()) else {
                continue;
            };
            t.tokens += v["prompt_tokens"].as_u64().unwrap_or(0) + v["completion_tokens"].as_u64().unwrap_or(0);
            t.secs += v["duration_secs"].as_f64().unwrap_or(0.0);
        }
        out.entry(task.to_string()).or_default().push(t);
    }
    Ok(out)
}

fn find(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else { return out };
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

fn rows(job: &Path) -> Result<Vec<Row>> {
    let cat: BTreeMap<String, Task> =
        catalog(&eval_dir()).unwrap_or_default().into_iter().map(|t| (t.name.clone(), t)).collect();
    Ok(trials_by_task(job)?
        .into_iter()
        .map(|(task, trials)| {
            let n = trials.len().max(1) as f64;
            let c = cat.get(&task);
            Row {
                capability: c.map(|c| c.capability.clone()).unwrap_or_else(|| "-".into()),
                difficulty: c.map(|c| c.difficulty.clone()).unwrap_or_else(|| "-".into()),
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
    if s >= 60 { format!("{}m{:02}s", s / 60, s % 60) } else { format!("{s}s") }
}

/// Prints the job like `cargo test`, writes summary.md/summary.json into it, and fails
/// when a task passed no trial.
fn report(job: &Path) -> Result<i32> {
    let rows = rows(job)?;
    let mut md = String::from("| task | capability | difficulty | passed | avg tokens | avg time | errors |\n|---|---|---|---|---|---|---|\n");
    let mut by_cap: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for r in &rows {
        let errors = if r.errors.is_empty() { String::new() } else { format!(" [{}]", r.errors.join(", ")) };
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
        println!("{cap:<14} {p}/{t} ({:.0}%)", 100.0 * *p as f64 / (*t).max(1) as f64);
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
    fs::write(job.join("summary.json"), serde_json::to_string_pretty(&json)?)?;
    Ok(if failed == 0 { 0 } else { 1 })
}

fn job_dir(name: Option<&String>) -> Result<PathBuf> {
    let jobs = eval_dir().join("jobs");
    match name.map(String::as_str) {
        Some(n) if n != "latest" => Ok(if Path::new(n).is_dir() { PathBuf::from(n) } else { jobs.join(n) }),
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
    let [a, b] = o.positional.as_slice() else { bail!("usage: cargo eval compare JOB_A JOB_B") };
    let ra: BTreeMap<String, Row> = rows(&job_dir(Some(a))?)?.into_iter().map(|r| (r.task.clone(), r)).collect();
    let rb: BTreeMap<String, Row> = rows(&job_dir(Some(b))?)?.into_iter().map(|r| (r.task.clone(), r)).collect();
    let rate = |r: &Row| r.passed as f64 / r.total.max(1) as f64;
    println!("{:<60} {:>9} {:>9} {:>10} {:>10}", "task", "a", "b", "a tok", "b tok");
    let names: std::collections::BTreeSet<&String> = ra.keys().chain(rb.keys()).collect();
    for name in names {
        let (x, y) = (ra.get(name), rb.get(name));
        let cell = |r: Option<&Row>| r.map(|r| format!("{}/{}", r.passed, r.total)).unwrap_or("-".into());
        let tok = |r: Option<&Row>| r.map(|r| human_tokens(r.tokens)).unwrap_or("-".into());
        let mark = match (x, y) {
            (Some(x), Some(y)) if rate(y) > rate(x) => " +",
            (Some(x), Some(y)) if rate(y) < rate(x) => " -",
            _ => "",
        };
        println!("{name:<60} {:>9} {:>9} {:>10} {:>10}{mark}", cell(x), cell(y), tok(x), tok(y));
    }
    Ok(0)
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from).with_context(|| format!("read {}", from.display()))? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir(&e.path(), &dest)?;
        } else {
            fs::copy(e.path(), dest)?;
        }
    }
    Ok(())
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
        assert_eq!(cat, known, "eval/catalog.toml does not match eval/tasks/ + eval/dataset.toml");
        for name in local.keys() {
            assert!(!registry.contains_key(name), "{name} is both hand-authored and in dataset.toml");
        }
    }

    #[test]
    fn materialize_links_local_and_pinned_tasks() {
        let tasks = [
            Task { name: "genji/x".into(), path: Some("tasks/a/x".into()), ..Default::default() },
            Task { name: "org/y".into(), digest: Some("sha256:ab".into()), ..Default::default() },
        ];
        let s = materialize("j", &tasks);
        assert!(s.contains("ln -sfn '../../tasks/a/x' 'genji__x'"));
        assert!(s.contains("harbor download 'org/y@sha256:ab' -o ../../.store/ab"));
        assert!(s.contains("ln -sfn '../../.store/ab/y' 'org__y'"));
    }

    #[test]
    fn catalog_entries_are_complete() {
        for t in catalog(&eval_dir()).unwrap() {
            assert!(!t.capability.is_empty(), "{}: no capability", t.name);
            assert!(["easy", "medium"].contains(&t.difficulty.as_str()), "{}: difficulty {:?}", t.name, t.difficulty);
        }
    }

    /// Every step of a hand-authored task has a prompt, a verifier and an oracle solution,
    /// and its front matter parses.
    #[test]
    fn local_tasks_are_well_formed() {
        for (name, dir) in local_tasks(&eval_dir()).unwrap() {
            let t = load_toml(&dir.join("task.toml")).unwrap();
            let steps: Vec<PathBuf> = match t.get("steps").and_then(|s| s.as_array()) {
                Some(steps) => steps.iter().map(|s| dir.join("steps").join(s["name"].as_str().unwrap())).collect(),
                None => vec![dir.clone()],
            };
            for s in steps {
                for f in ["instruction.md", "tests/test.sh", "solution/solve.sh"] {
                    assert!(s.join(f).is_file(), "{name}: missing {}", s.join(f).display());
                }
                let text = fs::read_to_string(s.join("instruction.md")).unwrap();
                if let Some(rest) = text.strip_prefix("+++\n") {
                    let (head, _) = rest.split_once("\n+++\n").unwrap_or_else(|| panic!("{name}: unclosed front matter"));
                    head.parse::<toml::Table>().unwrap_or_else(|e| panic!("{name}: front matter: {e}"));
                }
            }
        }
    }

    #[test]
    fn verifier_helpers_are_current() {
        let lib = fs::read_to_string(eval_dir().join("lib/verify.sh")).unwrap();
        for copy in find(&eval_dir().join("tasks"), "lib.sh") {
            assert_eq!(fs::read_to_string(&copy).unwrap(), lib, "{} differs from eval/lib/verify.sh", copy.display());
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
        let t = |n: &str, c: &str, d: &str| Task { name: n.into(), capability: c.into(), difficulty: d.into(), ..Default::default() };
        let all = vec![t("genji/needle", "long-output", "easy"), t("quixbugs/python-gcd", "instruction", "medium")];
        let o = Opts { positional: vec!["long".into()], ..Default::default() };
        assert_eq!(select(&o, all.clone()).len(), 1);
        let o = Opts { difficulty: Some("medium".into()), ..Default::default() };
        assert_eq!(select(&o, all)[0].name, "quixbugs/python-gcd");
    }
}
