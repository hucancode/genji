//! genji-drive STEP_JSON...: runs genji through a sequence of steps, in a Harbor task
//! container or in a long-running remote run (`cargo eval lp`).
//!
//! It holds genji's stdin and stdout: it injects instructions after N tool calls, kills
//! genji to simulate a crash, follows `finish` handoffs and blocked reports, resumes the
//! previous step's instance, and leaves the trace and metrics (in genji's own terms) in the
//! step's output directory. Translations to other harnesses' formats live in the Python
//! adapter.
//!
//! Layout under `$GENJI_DRIVE_HOME` (default /genji): config.json, sessions/, agents/,
//! state.json (the instance a later step resumes), phase (the running step's name, then
//! `done`) and trace/all.jsonl (every step's events, for verifiers). `$GENJI_BIN` is the
//! genji binary (default `genji` on PATH).

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

#[derive(Deserialize)]
struct Step {
    /// Names the step's files (`<name>.jsonl`, `<name>.err`, `<name>.metrics.json`) and the phase.
    name: Option<String>,
    workdir: String,
    #[serde(default = "default_agent")]
    agent: String,
    prompt: String,
    #[serde(default)]
    resume: bool,
    #[serde(default)]
    follow_handoffs: u32,
    /// Shell commands run in the workdir before and after the step.
    before: Option<String>,
    after: Option<String>,
    token_limit: Option<u64>,
    #[serde(default)]
    instructions: Vec<Instruction>,
    kill_after_tool_calls: Option<u64>,
    out: String,
    /// Copy config.json, sessions/ and agents/ into `out` after the step.
    #[serde(default = "yes")]
    snapshot: bool,
}

#[derive(Deserialize)]
struct Instruction {
    after_tool_calls: u64,
    text: String,
}

fn default_agent() -> String {
    "build".into()
}

fn yes() -> bool {
    true
}

#[derive(Default)]
struct Metrics {
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    tool_calls: u64,
    tool_errors: u64,
    spawns: u64,
    asks: u64,
    compactions: u64,
    prunes: u64,
    errors: u64,
    instances: u64,
    killed: bool,
    end: Option<Value>,
}

impl Metrics {
    fn count(&mut self, e: &Value) {
        let n = |k: &str| e[k].as_u64().unwrap_or(0);
        match e["type"].as_str().unwrap_or_default() {
            "tokens" => {
                self.prompt_tokens += n("prompt");
                self.completion_tokens += n("completion");
                self.cached_tokens += n("cached");
            }
            "tool_call" => {
                self.tool_calls += 1;
                match e["name"].as_str() {
                    Some("spawn") => self.spawns += 1,
                    Some("ask") => self.asks += 1,
                    _ => {}
                }
            }
            "tool_result" if e["is_error"].as_bool() == Some(true) => self.tool_errors += 1,
            "compaction" => self.compactions += 1,
            "prune" => self.prunes += 1,
            "error" => self.errors += 1,
            "instance_start" => self.instances += 1,
            "instance_end" => self.end = Some(e.clone()),
            _ => {}
        }
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("GENJI_DRIVE_HOME").unwrap_or_else(|_| "/genji".into()))
}

fn main() {
    let mut paths: Vec<String> = std::env::args().skip(1).collect();
    if paths.is_empty() {
        paths.push(home().join("step.json").to_string_lossy().into_owned());
    }
    let mut code = 0;
    for path in &paths {
        match run(path) {
            Ok(c) => code = c,
            Err(e) => {
                eprintln!("genji-drive: {e:#}");
                std::process::exit(3);
            }
        }
    }
    if paths.len() > 1 {
        let _ = fs::write(home().join("phase"), "done\n");
    }
    std::process::exit(code);
}

fn shell(cmd: &str, dir: &str) -> Result<()> {
    let ok = Command::new("sh").arg("-c").arg(cmd).current_dir(dir).status()?.success();
    anyhow::ensure!(ok, "command failed: {cmd}");
    Ok(())
}

fn run(path: &str) -> Result<i32> {
    let step: Step = serde_json::from_str(&fs::read_to_string(path).with_context(|| format!("read {path}"))?)
        .with_context(|| format!("parse {path}"))?;
    let home = home();
    let out = Path::new(&step.out);
    let file = |suffix: &str, default: &str| match &step.name {
        Some(n) => out.join(format!("{n}{suffix}")),
        None => out.join(default),
    };
    fs::create_dir_all(out)?;
    fs::create_dir_all(home.join("trace"))?;
    fs::create_dir_all(home.join("sessions"))?;
    if let Some(n) = &step.name {
        fs::write(home.join("phase"), format!("{n}\n"))?;
    }
    let started = Instant::now();
    if let Some(cmd) = &step.before {
        shell(cmd, &step.workdir)?;
    }

    let state_path = home.join("state.json");
    let state: Value = fs::read_to_string(&state_path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let mut agent = step.agent.clone();
    let mut task = step.prompt.clone();
    let mut resume = None;
    if step.resume {
        resume = state["instance"].as_str().map(String::from);
        if let Some(a) = state["agent"].as_str() {
            agent = a.to_string();
        }
    }
    let genji = std::env::var("GENJI_BIN").unwrap_or_else(|_| "genji".into());
    let config = home.join("config.json");
    let mut parent: Option<String> = None;
    let mut metrics = Metrics::default();
    let mut code = 0;
    let mut current: Option<(String, String)> = None;

    let mut trace = append(&file(".jsonl", "genji.jsonl"))?;
    let mut all = append(&home.join("trace/all.jsonl"))?;
    for hop in 0..=step.follow_handoffs {
        let mut cmd = Command::new(&genji);
        cmd.arg(&agent).arg(&task);
        if let Some(id) = resume.take() {
            cmd.arg("--resume").arg(id);
        }
        if let Some(p) = &parent {
            cmd.arg("--parent").arg(p);
        }
        if let Some(n) = step.token_limit {
            cmd.arg("--token-limit").arg(n.to_string());
        }
        cmd.arg("--config")
            .arg(&config)
            .arg("--socket-disabled")
            .current_dir(&step.workdir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(append(&file(".err", "genji.stderr.log"))?);
        let mut child = cmd.spawn().with_context(|| format!("start {genji}"))?;
        let mut stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().context("genji stdout")?);
        let mut pending: Vec<&Instruction> = step.instructions.iter().collect();
        let mut end = None;
        for line in stdout.lines() {
            let line = line?;
            writeln!(trace, "{line}")?;
            writeln!(all, "{line}")?;
            let Ok(e) = serde_json::from_str::<Value>(&line) else { continue };
            metrics.count(&e);
            match e["type"].as_str().unwrap_or_default() {
                // A review pass is not resumable on its own; the work instance is.
                "instance_start" if !e["agent"].as_str().unwrap_or_default().ends_with(":review") => {
                    current = Some((
                        e["instance"].as_str().unwrap_or_default().to_string(),
                        e["agent"].as_str().unwrap_or_default().to_string(),
                    ));
                }
                "instance_end" => end = Some(e.clone()),
                "tool_call" => {
                    let calls = metrics.tool_calls;
                    if let Some(w) = stdin.as_mut() {
                        for i in pending.iter().filter(|i| i.after_tool_calls <= calls) {
                            let cmd = json!({"type": "instruction", "text": i.text});
                            writeln!(w, "{cmd}")?;
                            w.flush()?;
                        }
                    }
                    pending.retain(|i| i.after_tool_calls > calls);
                    if step.kill_after_tool_calls.is_some_and(|n| calls >= n) {
                        child.kill()?;
                        metrics.killed = true;
                        break;
                    }
                }
                _ => {}
            }
        }
        drop(stdin);
        let status = child.wait()?;
        code = status.code().unwrap_or(137);
        if metrics.killed || hop == step.follow_handoffs {
            break;
        }
        // A handoff continues with the named agent, a blocked report with the same agent.
        let r = end.as_ref().map(|e| e["result"].clone()).unwrap_or(Value::Null);
        let Some((id, _)) = &current else { break };
        match (r["status"].as_str(), r["next"]["agent"].as_str()) {
            (Some("handoff"), Some(next)) => {
                agent = next.to_string();
                task = r["next"]["task"].as_str().unwrap_or_default().to_string();
            }
            (Some("blocked"), _) if r["summary"].as_str().is_some_and(|s| !s.is_empty()) => {
                task = format!(
                    "Continue from this report of the previous instance:\n\n{}",
                    r["summary"].as_str().unwrap_or_default()
                );
            }
            _ => break,
        }
        parent = Some(id.clone());
    }

    if let Some((id, a)) = &current {
        fs::write(&state_path, json!({"instance": id, "agent": a}).to_string())?;
    }
    if let Some(cmd) = &step.after {
        shell(cmd, &step.workdir)?;
    }
    let end = metrics.end.clone().unwrap_or(Value::Null);
    let summary = json!({
        "exit_code": code,
        "killed": metrics.killed,
        "status": end["status"],
        "reason": end["reason"],
        "result": end["result"],
        "report": end["report"],
        "tokens_used": end["tokens_used"],
        "prompt_tokens": metrics.prompt_tokens,
        "completion_tokens": metrics.completion_tokens,
        "cached_tokens": metrics.cached_tokens,
        "tool_calls": metrics.tool_calls,
        "tool_errors": metrics.tool_errors,
        "spawns": metrics.spawns,
        "asks": metrics.asks,
        "compactions": metrics.compactions,
        "prunes": metrics.prunes,
        "errors": metrics.errors,
        "instances": metrics.instances,
        "duration_secs": started.elapsed().as_secs_f64(),
    });
    fs::write(file(".metrics.json", "metrics.json"), serde_json::to_string_pretty(&summary)?)?;
    if step.snapshot {
        let _ = fs::copy(&config, out.join("config.json"));
        copy_dir(&home.join("sessions"), &out.join("sessions"))?;
        copy_dir(&home.join("agents"), &out.join("agents"))?;
    }
    Ok(code)
}

fn append(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    let Ok(entries) = fs::read_dir(from) else { return Ok(()) };
    fs::create_dir_all(to)?;
    for entry in entries {
        let entry = entry?;
        let dest = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dest)?;
        } else {
            fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}
