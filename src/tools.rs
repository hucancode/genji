use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::agent::Agent;
use crate::config::{AgentDef, DEFAULT_FINISH};
use crate::storage::events;
use crate::storage::proc;
use crate::storage::util::{TempPath, relative_path, sanitize, tmp_file, write_file};

/// Deserialize a tool payload.
fn parse_args<'a, T: Deserialize<'a>>(args: &'a Value) -> Result<T> {
    T::deserialize(args).context("invalid tool arguments")
}

fn read_text(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
}

// --- files ---------------------------------------------------------------

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}

/// Whole-file reads are capped; larger files are paged with offset/limit.
const DEFAULT_READ_LINES: usize = 500;

fn read(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: ReadArgs = parse_args(args)?;
    let path = agent.resolve_path(&a.path);
    let content = read_text(&path)?;
    let total = content.lines().count();
    if total == 0 {
        return Ok(format!("{} is empty (0 lines)", path.display()));
    }
    let start = (a.offset.unwrap_or(1).max(1) - 1).min(total);
    let end = (start + a.limit.unwrap_or(DEFAULT_READ_LINES).max(1)).min(total);
    let mut out = String::new();
    for (i, line) in content.lines().enumerate().take(end).skip(start) {
        writeln!(out, "{:>6}\t{line}", i + 1)?;
    }
    if end < total {
        writeln!(
            out,
            "\n[showing lines {}-{end} of {total}; pass offset/limit for the rest]",
            start + 1
        )?;
    }
    Ok(out)
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

fn write(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: WriteArgs = parse_args(args)?;
    let path = agent.resolve_path(&a.path);
    write_file(&path, &a.content)?;
    Ok(format!(
        "wrote {} bytes to {}",
        a.content.len(),
        agent.display_path(&path)
    ))
}

/// Byte range of the one line window in `hay` equal to `needle` line by line,
/// ignoring leading and trailing whitespace on each line.
fn find_fuzzy(hay: &str, needle: &str) -> Option<(usize, usize)> {
    let want: Vec<&str> = needle.trim_matches('\n').lines().map(str::trim).collect();
    let mut lines = Vec::new();
    let mut at = 0;
    for l in hay.split_inclusive('\n') {
        let indent = l.len() - l.trim_start().len();
        lines.push((at + indent, at + l.trim_end().len().max(indent), l.trim()));
        at += l.len();
    }
    let mut hits = lines
        .windows(want.len())
        .filter(|w| w.iter().map(|l| l.2).eq(want.iter().copied()));
    match (hits.next(), hits.next()) {
        (Some(w), None) => Some((w[0].0, w[w.len() - 1].1)),
        _ => None,
    }
}

/// The byte range of `needle` in `hay`. An exact match is preferred; when there is none and
/// `all` is false, a whitespace-insensitive line window is tried. With `all`, a non-unique
/// match is allowed (the caller replaces every occurrence).
fn find_match(hay: &str, needle: &str, all: bool) -> Result<(usize, usize)> {
    if needle.is_empty() {
        bail!("oldText must not be empty");
    }
    let mut it = hay.match_indices(needle);
    let shown = || needle.chars().take(60).collect::<String>();
    match (it.next(), it.next()) {
        (Some((i, _)), None) => Ok((i, i + needle.len())),
        (Some((i, _)), Some(_)) if all => Ok((i, i + needle.len())),
        (Some(_), Some(_)) => bail!("oldText is not unique: {:?}", shown()),
        (None, _) if all => bail!("oldText not found: {:?}", shown()),
        (None, _) => {
            find_fuzzy(hay, needle).ok_or_else(|| anyhow!("oldText not found: {:?}", shown()))
        }
    }
}

fn apply_edits(content: &str, edits: &[(String, String)], replace_all: bool) -> Result<String> {
    if let [(old, new)] = edits
        && replace_all
    {
        find_match(content, old, true)?;
        return Ok(content.replace(old.as_str(), new));
    }
    let mut ranges = Vec::new();
    for (old, new) in edits {
        let (start, end) = find_match(content, old, false)?;
        ranges.push((start, end, new));
    }
    ranges.sort_by_key(|r| r.0);
    if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
        bail!("edit ranges overlap");
    }
    let mut out = content.to_string();
    for (start, end, new) in ranges.into_iter().rev() {
        out.replace_range(start..end, new);
    }
    Ok(out)
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    edits: Option<Vec<EditEntry>>,
    #[serde(rename = "oldText", alias = "old_text")]
    old_text: Option<String>,
    #[serde(rename = "newText", alias = "new_text")]
    new_text: Option<String>,
    replace_all: Option<bool>,
}

#[derive(Deserialize)]
struct EditEntry {
    #[serde(rename = "oldText", alias = "old_text")]
    old_text: String,
    #[serde(rename = "newText", alias = "new_text")]
    new_text: String,
}

fn edit(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: EditArgs = parse_args(args)?;
    let path = agent.resolve_path(&a.path);
    let content = read_text(&path)?;
    let mut edits: Vec<(String, String)> = a
        .edits
        .unwrap_or_default()
        .into_iter()
        .map(|e| (e.old_text, e.new_text))
        .collect();
    if let (true, Some(old), Some(new)) = (edits.is_empty(), a.old_text, a.new_text) {
        edits.push((old, new));
    }
    if edits.is_empty() {
        bail!("no edits supplied (provide `edits` array or `oldText`/`newText`)");
    }
    let new_content = apply_edits(&content, &edits, a.replace_all.unwrap_or(false))?;
    write_file(&path, &new_content)?;
    Ok(format!(
        "applied {} edit(s) to {} ({} -> {} bytes)",
        edits.len(),
        agent.display_path(&path),
        content.len(),
        new_content.len()
    ))
}

#[derive(Deserialize)]
struct LsArgs {
    path: Option<String>,
    show_hidden: Option<bool>,
    max_depth: Option<usize>,
}

/// Names in `dir` that git ignores (none outside a repository).
fn git_ignored(dir: &Path, names: &[String]) -> HashSet<String> {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    let run = || -> Result<HashSet<String>> {
        let mut child = Command::new("git")
            .args(["check-ignore", "--stdin"])
            .current_dir(dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().context("git stdin")?;
        let input = names.join("\n");
        // Written on a thread so a long listing cannot deadlock against git's output.
        let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
        let out = child.wait_with_output()?;
        let _ = writer.join();
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(str::to_string)
            .collect())
    };
    run().unwrap_or_default()
}

fn walk(
    dir: &Path,
    depth: usize,
    hidden: bool,
    lines: &mut Vec<String>,
    counts: &mut (usize, usize),
    base: &Path,
) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name != ".git" && (hidden || !name.starts_with('.'))
        })
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let names: Vec<String> = entries
        .iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let ignored = git_ignored(dir, &names);
    for (e, name) in entries.into_iter().zip(&names) {
        if ignored.contains(name) {
            continue;
        }
        let path = e.path();
        let shown = relative_path(base, &path);
        match e.metadata() {
            Ok(m) if m.is_dir() => {
                counts.0 += 1;
                lines.push(format!("d        {shown}/"));
                if depth > 0 {
                    walk(&path, depth - 1, hidden, lines, counts, base);
                }
            }
            Ok(m) => {
                counts.1 += 1;
                lines.push(format!("f {:>8} {shown}", m.len()));
            }
            Err(_) => {}
        }
    }
}

fn ls(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: LsArgs = parse_args(args)?;
    let rel = a.path.unwrap_or_else(|| ".".into());
    let root = agent.resolve_path(&rel);
    if !root.is_dir() {
        bail!("not a directory: {}", root.display());
    }
    let (mut lines, mut counts) = (Vec::new(), (0, 0));
    walk(
        &root,
        a.max_depth.unwrap_or(0),
        a.show_hidden.unwrap_or(false),
        &mut lines,
        &mut counts,
        &agent.workspace,
    );
    let body = if lines.is_empty() {
        "(empty)".to_string()
    } else {
        lines.join("\n")
    };
    Ok(format!(
        "{body}\n\n[{} dirs, {} files under {rel}]",
        counts.0, counts.1
    ))
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    cwd: Option<String>,
    timeout_secs: Option<i64>,
}

fn bash(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: BashArgs = parse_args(args)?;
    let cwd = a
        .cwd
        .map_or_else(|| agent.workspace.clone(), |c| agent.resolve_path(&c));
    let default = agent.cfg.bash_timeout_secs;
    let timeout = Duration::from_secs(
        a.timeout_secs
            .map_or(default, |v| u64::try_from(v.max(1)).unwrap_or(default)),
    );
    let cap = agent.cfg.tool_result_max_bytes.saturating_mul(2).max(8192);
    let res = proc::run_capture(
        "bash",
        &["-c".into(), a.command.clone()],
        &cwd,
        &std::env::temp_dir(),
        timeout,
        cap,
    )
    .with_context(|| format!("running command: {}", a.command))?;
    let mut out = format!(
        "exit_code: {}\n",
        res.code.map_or_else(|| "none".into(), |c| c.to_string())
    );
    if res.timed_out {
        let _ = writeln!(out, "[timed out after {}s]", timeout.as_secs());
    }
    for (label, text) in [("stdout", &res.stdout), ("stderr", &res.stderr)] {
        if !text.is_empty() {
            let _ = write!(out, "--- {label} ---\n{text}");
            if !text.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    Ok(out)
}

// --- ask ------------------------------------------------------------------

#[derive(Deserialize)]
struct AskArgs {
    question: String,
    #[serde(deserialize_with = "string_list")]
    options: Vec<String>,
    recommended: String,
}

/// A list of strings, also accepted as a JSON-encoded array in a string, which some
/// models emit for array arguments. Anything else says what shape is expected.
fn string_list<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    use serde::de::Error;
    match Value::deserialize(d)? {
        Value::Array(items) => items
            .into_iter()
            .map(|v| {
                v.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| D::Error::custom("options must be an array of strings"))
            })
            .collect(),
        Value::String(s) => serde_json::from_str::<Vec<String>>(&s).map_err(|_| {
            D::Error::custom(
                "options must be a JSON array of 2-6 strings, e.g. [\"A\", \"B\"], not a string",
            )
        }),
        _ => Err(D::Error::custom(
            "options must be a JSON array of 2-6 strings",
        )),
    }
}

/// Ask the human a multiple-choice question and block on the answer.
fn ask(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: AskArgs = parse_args(args)?;
    if a.question.trim().is_empty() {
        bail!("question is empty");
    }
    if !(2..=6).contains(&a.options.len()) {
        bail!("options must hold 2 to 6 choices, got {}", a.options.len());
    }
    if !a.options.contains(&a.recommended) {
        bail!("recommended must be one of options");
    }
    agent.ask(&a.options, &a.recommended)
}

// --- finish ---------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Next {
    pub agent: String,
    pub task: String,
}

/// What an agent declares when it stops.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub status: String,
    #[serde(default)]
    pub summary: String,
    pub next: Option<Next>,
}

/// The statuses `finish` offers: the agent's own list, and for a subagent only
/// those that report back (at least `handoff`).
pub fn finish_statuses(def: &AgentDef, subagent: bool) -> Vec<String> {
    let mut s: Vec<String> = def
        .finish
        .iter()
        .filter(|s| !subagent || *s != "done")
        .cloned()
        .collect();
    if s.is_empty() {
        s.push("handoff".into());
    }
    s
}

fn validate_finish(
    def: &AgentDef,
    agents: &BTreeMap<String, AgentDef>,
    parent: Option<&str>,
    args: &Value,
) -> Result<Verdict> {
    let mut v: Verdict = parse_args(args)?;
    // A handoff's `next.task` is its report; auto fill summary if none
    if v.summary.trim().is_empty()
        && v.status == "handoff"
        && let Some(n) = &v.next
    {
        v.summary = format!("handed off to {}", n.agent);
    }
    let allowed = finish_statuses(def, parent.is_some());
    if !allowed.contains(&v.status) {
        bail!("status must be one of: {}", allowed.join(", "));
    }
    if v.summary.trim().is_empty() {
        bail!("summary must not be empty");
    }
    match (&v.next, v.status == "handoff") {
        (None, true) => bail!("status handoff needs `next` {{agent, task}}"),
        (Some(_), false) => bail!("`next` is only for status handoff"),
        (Some(n), true) => validate_handoff(def, agents, parent, n)?,
        (None, false) => {}
    }
    Ok(v)
}

/// The target a handoff continues with: the agent exists, the task stands alone, and a
/// top-level handoff respects the `spawns:` gate. A subagent may only report to its parent.
fn validate_handoff(
    def: &AgentDef,
    agents: &BTreeMap<String, AgentDef>,
    parent: Option<&str>,
    next: &Next,
) -> Result<()> {
    if !agents.contains_key(&next.agent) {
        bail!(
            "unknown agent `{}`; available: {}",
            next.agent,
            agents.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if next.task.trim().is_empty() {
        bail!("next.task must not be empty");
    }
    match parent {
        Some(p) if p != next.agent => bail!("a subagent hands off to its parent agent `{p}`"),
        Some(_) => Ok(()),
        None => check_may_spawn(def, agents, &next.agent),
    }
}

/// Record what the run declares when it stops; only one may be recorded.
fn claim_verdict(agent: &mut Agent, v: Verdict, handed_off: bool) -> Result<String> {
    if agent.verdict.is_some() {
        bail!("a verdict was already recorded");
    }
    agent.handed_off = handed_off;
    agent.verdict = Some(v);
    Ok("ok".into())
}

fn finish(agent: &mut Agent, args: &Value) -> Result<String> {
    let v = validate_finish(
        &agent.def,
        &agent.agents,
        agent.parent_agent.as_deref(),
        args,
    )?;
    claim_verdict(agent, v, false)
}

#[derive(Deserialize)]
struct HandOffArgs {
    agent: String,
    task: String,
}

/// Fails unless this agent's definition allows starting `target` (`spawns:`; default: every agent).
fn check_may_spawn(
    def: &AgentDef,
    agents: &BTreeMap<String, AgentDef>,
    target: &str,
) -> Result<()> {
    if agents.contains_key(target) && def.may_spawn(target) {
        return Ok(());
    }
    let allowed: Vec<_> = agents
        .keys()
        .filter(|n| def.may_spawn(n))
        .cloned()
        .collect();
    bail!("cannot spawn `{target}`; available: {}", allowed.join(", "))
}

/// Ends this run; genji continues with a fresh instance of `agent` working on `task`.
fn hand_off(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: HandOffArgs = parse_args(args)?;
    if agent.parent_agent.is_some() {
        bail!("a subagent reports with `finish`, not `hand_off`");
    }
    let next = Next {
        agent: a.agent,
        task: a.task,
    };
    validate_handoff(&agent.def, &agent.agents, None, &next)?;
    claim_verdict(
        agent,
        Verdict {
            status: "handoff".into(),
            summary: format!("handed off to {}", next.agent),
            next: Some(next),
        },
        true,
    )
}

#[derive(Deserialize)]
struct VerdictArgs {
    verdict: String,
    notes: String,
}

/// Ends a review pass. `handoff` continues in a fresh work instance with `notes` as its task;
/// `reject` returns `notes` to the work pass.
fn verdict(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: VerdictArgs = parse_args(args)?;
    if a.notes.trim().is_empty() {
        bail!("notes must not be empty");
    }
    let next = match a.verdict.as_str() {
        "done" | "reject" | "blocked" => None,
        "handoff" => Some(Next {
            agent: agent.def.worker().to_string(),
            task: a.notes.clone(),
        }),
        v => bail!("verdict must be one of: done, reject, handoff, blocked (got `{v}`)"),
    };
    let handed_off = next.is_some();
    claim_verdict(
        agent,
        Verdict {
            status: a.verdict,
            summary: a.notes,
            next,
        },
        handed_off,
    )
}

// --- spawn ----------------------------------------------------------------

#[derive(Deserialize)]
struct SpawnArgs {
    agent: String,
    instructions: String,
}

/// The `spawn` tool result for a finished child: its handoff (or a `blocked`
/// report when it ended without one), built from its event lines.
fn child_report(child: &str, agent: &str, events: &[Value], timed_out: bool, cap: usize) -> String {
    let end = events.iter().rev().find(|e| e["type"] == "instance_end");
    let last_text = events
        .iter()
        .rev()
        .find(|e| {
            e["type"] == "assistant" && !e["content"].as_str().unwrap_or("").trim().is_empty()
        })
        .and_then(|e| e["content"].as_str());
    let run = if timed_out {
        "timed_out"
    } else {
        end.and_then(|e| e["status"].as_str())
            .unwrap_or("incomplete")
    };
    let result = end
        .map(|e| &e["result"])
        .filter(|r| matches!(r["status"].as_str(), Some("handoff" | "blocked")));
    let (status, report) = match result {
        Some(r) if r["status"] == "handoff" => (
            "handoff",
            r["next"]["task"].as_str().unwrap_or_default().to_string(),
        ),
        Some(r) => (
            "blocked",
            r["summary"].as_str().unwrap_or_default().to_string(),
        ),
        None => {
            let last = last_text
                .or_else(|| end.and_then(|e| e["report"].as_str()))
                .unwrap_or("(no output)");
            (
                "blocked",
                format!("subagent ended without a handoff ({run}); last output: {last}"),
            )
        }
    };
    json!({ "subagent": child, "agent": agent, "status": status, "report": crate::storage::util::truncate(&report, cap), "run": run }).to_string()
}

/// Run a child genji and return its handoff. The child id is derived from the
/// call, so a resumed parent finds the child's session: a finished one is
/// delivered as is, an unfinished one is stopped and resumed.
fn spawn(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: SpawnArgs = parse_args(args)?;
    check_may_spawn(&agent.def, &agent.agents, &a.agent)?;
    if agent.depth >= agent.cfg.max_subagent_depth {
        bail!(
            "subagent depth limit reached ({} >= {})",
            agent.depth,
            agent.cfg.max_subagent_depth
        );
    }
    let child = format!("{}-{}", agent.instance_id, sanitize(&agent.call_id));
    let cap = agent
        .cfg
        .tool_result_max_bytes
        .saturating_sub(1024)
        .max(4096);
    let sessions = agent.cfg.sessions(&agent.workspace);
    let session = sessions.join(format!("{child}.jsonl"));
    let tmp = std::env::temp_dir();
    let instructions = TempPath(tmp_file(&tmp, "subagent", "md"));
    let mut cmd: Vec<String> = [
        "--subagent",
        "--parent",
        &agent.instance_id,
        "--parent-agent",
        &agent.def.name,
        "--depth",
        &(agent.depth + 1).to_string(),
    ]
    .map(String::from)
    .into();
    // The child's own socket, next to this one; none when this run has none.
    match &agent.control.path {
        Some(p) => {
            let socket = p.with_file_name(format!("control-{child}.sock"));
            cmd.extend(["--socket".into(), socket.to_string_lossy().into()]);
        }
        None => cmd.push("--socket-disabled".into()),
    }
    // The child runs on this run's effective config, overrides included. It goes through a
    // private file rather than argv, which other users can read.
    let mut cfg = agent.cfg.clone();
    cfg.sessions_dir = Some(sessions.clone());
    cfg.agents_dir = Some(agent.cfg.agents(&agent.workspace));
    let config = TempPath(tmp_file(&tmp, "subagent-config", "json"));
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&config.0)
            .context("creating subagent config")?;
        std::io::Write::write_all(&mut f, serde_json::to_string(&cfg)?.as_bytes())?;
    }
    cmd.extend(["--config".into(), config.0.to_string_lossy().into()]);
    if session.exists() {
        let past = events::read(&session)?;
        if past.iter().any(|e| e["type"] == "instance_end") {
            return Ok(child_report(&child, &a.agent, &past, false, cap));
        }
        let pid = past.first().and_then(|e| e["pid"].as_u64());
        if let Some(pid) = pid
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| proc::alive(*p))
        {
            proc::kill_group(pid);
        }
        cmd.extend(["--resume".into(), child.clone()]);
    } else {
        write_file(&instructions.0, &a.instructions)?;
        cmd.extend([
            a.agent.clone(),
            "--instance-id".into(),
            child.clone(),
            "--instructions-file".into(),
            instructions.0.to_string_lossy().into(),
        ]);
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| "genji".into());
    // The child records its events in its session file; its stdout is not needed.
    let res = proc::run_capture(
        &exe.to_string_lossy(),
        &cmd,
        &agent.workspace,
        &tmp,
        Duration::from_secs(agent.cfg.spawn_timeout_secs),
        1,
    )?;
    let events = events::read(&session).unwrap_or_default();
    Ok(child_report(&child, &a.agent, &events, res.timed_out, cap))
}

// --- registry -------------------------------------------------------------

type Handler = fn(&mut Agent, &Value) -> Result<String>;

/// A tool: name, description, JSON-schema parameters and handler.
struct Tool(&'static str, &'static str, Value, Handler);

fn registry() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        vec![
            Tool("read", "Read a text file with line numbers; offset is 1-indexed. Prefer this over cat/sed/head/tail in bash for reading files: page large files with offset/limit.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","description":"File path"},
                    "offset":{"type":"integer","description":"First line (1-indexed)"},
                    "limit":{"type":"integer","description":"Max lines to read (default 500)"}
                },
                "required":["path"]
            }), read),
            Tool("write", "Create or overwrite a file, creating parent directories.", json!({
                "type":"object",
                "properties":{"path":{"type":"string"},"content":{"type":"string"}},
                "required":["path","content"]
            }), write),
            Tool("edit", "Apply precise text replacements to a file. Each oldText must match uniquely.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "edits":{"type":"array","items":{"type":"object","properties":{
                        "oldText":{"type":"string"},"newText":{"type":"string"}
                    },"required":["oldText","newText"]}},
                    "oldText":{"type":"string","description":"Single-edit shorthand"},
                    "newText":{"type":"string","description":"Single-edit shorthand"},
                    "replace_all":{"type":"boolean","description":"Allow replacing a non-unique oldText (single-edit form)"}
                },
                "required":["path"]
            }), edit),
            Tool("ls", "List files/directories respecting .gitignore.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","description":"Directory (default .)"},
                    "max_depth":{"type":"integer","description":"Recursion depth; Default = 0 = lists only immediate children"},
                    "show_hidden":{"type":"boolean","description":"default false"}
                }
            }), ls),
            Tool("bash", "Run a shell command via bash -c in the workspace. Returns exit code, stdout, stderr.", json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string"},
                    "cwd":{"type":"string","description":"Working directory (default workspace)"},
                    "timeout_secs":{"type":"integer"}
                },
                "required":["command"]
            }), bash),
            Tool("ask", "Ask the human a multiple-choice question and wait for the answer. If nobody replies in time, the recommended option is used. A free-text reply is possible and comes back marked as such.", json!({
                "type":"object",
                "properties":{
                    "question":{"type":"string"},
                    "options":{"type":"array","items":{"type":"string"},"minItems":2,"maxItems":6},
                    "recommended":{"type":"string","description":"Your pick; must be one of options"}
                },
                "required":["question","options","recommended"]
            }), ask),
            Tool("spawn", "Run a subagent that works on the instructions and hands its report back as this call's result.", json!({
                "type":"object",
                "properties":{
                    "agent":{"type":"string","description":"Agent name"},
                    "instructions":{"type":"string","description":"Self-contained instructions for the subagent"}
                },
                "required":["agent","instructions"]
            }), spawn),
            Tool("hand_off", "Your context is getting heavy or a batch is done and work remains: end this run and continue in a fresh instance of `agent` (it may be yourself) with an empty context. `task` must stand alone: the goal, what is done, what is left, where the state lives (branch, files, failing test), and the assumptions so far.", json!({
                "type":"object",
                "properties":{"agent":{"type":"string"},"task":{"type":"string"}},
                "required":["agent","task"]
            }), hand_off),
            Tool("finish", "End your run. done: the goal is achieved and verified. handoff: your part is done and `next.agent` continues with `next.task` (self-contained: goal, what is done, what is left, where the state lives). blocked: a human must step in.", json!({
                "type":"object",
                "properties":{
                    "status":{"type":"string","enum":DEFAULT_FINISH},
                    "summary":{"type":"string","description":"What was done, the evidence, where the state lives"},
                    "next":{"type":"object","description":"Required for handoff only","properties":{
                        "agent":{"type":"string"},"task":{"type":"string"}
                    },"required":["agent","task"]}
                },
                "required":["status","summary"]
            }), finish),
            Tool("verdict", "End the review. done: the request is met and verified. reject: the work pass refines its work using `notes`. handoff: a separate part remains; a fresh work instance continues with `notes` as its task (self-contained: goal, what is done, what is left, where the state lives). blocked: a human must step in.", json!({
                "type":"object",
                "properties":{
                    "verdict":{"type":"string","enum":["done","reject","handoff","blocked"]},
                    "notes":{"type":"string","description":"done: the evidence. reject: the concrete problems. handoff: the standalone task. blocked: why"}
                },
                "required":["verdict","notes"]
            }), verdict),
        ]
    })
}

/// Fails when an agent lists a tool that does not exist or an invalid finish status.
pub fn check(def: &AgentDef) -> Result<()> {
    if let Some(t) = def
        .tools
        .iter()
        .find(|t| !registry().iter().any(|r| r.0 == t.as_str()))
    {
        bail!("unknown tool `{t}`");
    }
    if let Some(s) = def
        .finish
        .iter()
        .find(|s| !DEFAULT_FINISH.contains(&s.as_str()))
    {
        bail!("unknown finish status `{s}`");
    }
    Ok(())
}

/// Every tool as `{name, description, parameters}`.
pub fn list() -> Vec<Value> {
    registry()
        .iter()
        .map(|Tool(name, description, parameters, _)| {
            json!({"name": name, "description": description, "parameters": parameters})
        })
        .collect()
}

/// Tool definitions sent to the model for this agent.
pub fn specs(def: &AgentDef, subagent: bool) -> Vec<Value> {
    registry()
        .iter()
        .filter(|t| def.tools.iter().any(|n| n == t.0))
        .map(|Tool(name, description, parameters, _)| {
            let mut parameters = parameters.clone();
            if *name == "finish" {
                parameters["properties"]["status"]["enum"] = json!(finish_statuses(def, subagent));
            }
            json!({"type": "function", "function": {"name": name, "description": description, "parameters": parameters}})
        })
        .collect()
}

pub fn dispatch(agent: &mut Agent, name: &str, args: &Value) -> (String, bool) {
    let result = (|| {
        if !agent.def.tools.iter().any(|t| t == name) {
            bail!("tool `{name}` is not available to this agent");
        }
        let t = registry()
            .iter()
            .find(|t| t.0 == name)
            .ok_or_else(|| anyhow!("unknown tool `{name}`"))?;
        (t.3)(agent, args)
    })();
    let (text, is_error) = match result {
        Ok(s) => (s, false),
        Err(e) => (format!("ERROR: {e:#}"), true),
    };
    (
        bounded_result(
            &agent.workspace,
            agent.cfg.tool_result_max_bytes,
            name,
            text,
        ),
        is_error,
    )
}

/// Results over `max` bytes are clipped; the full text is spilled to a tmp file.
fn bounded_result(workspace: &Path, max: usize, name: &str, text: String) -> String {
    if text.len() <= max {
        return text;
    }
    // Failures and summaries come last in command output, so keep the tail as well.
    let clipped = if name == "bash" {
        crate::storage::util::head_and_tail(&text, max)
    } else {
        crate::storage::util::truncate(&text, max)
    };
    let path: PathBuf = tmp_file(
        &std::env::temp_dir(),
        &format!("tool-{}", sanitize(name)),
        "log",
    );
    match write_file(&path, &text) {
        Ok(()) => format!(
            "{clipped}\n[result ({} bytes) written to {}; read it with the read tool]",
            text.len(),
            relative_path(workspace, &path)
        ),
        Err(_) => clipped.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config;
    use crate::storage::util::temp_dir;

    /// Agent definitions parsed from `(name, file text)`, without touching disk.
    fn defs(files: &[(&str, &str)]) -> BTreeMap<String, AgentDef> {
        files
            .iter()
            .map(|(n, text)| (n.to_string(), config::parse_agent(n, text).0))
            .collect()
    }

    fn reviewed_build() -> AgentDef {
        AgentDef {
            name: "build".into(),
            prompt: "work".into(),
            tools: ["read", "write", "hand_off", "finish"]
                .map(String::from)
                .into(),
            finish: vec!["done".into()],
            review: Some("judge".into()),
            ..Default::default()
        }
    }

    fn agent_for(def: AgentDef) -> Agent {
        use std::sync::{Arc, RwLock};
        let context = Arc::new(RwLock::new(crate::storage::context::ContextComposer::new(
            String::new(),
            vec![],
            1000,
        )));
        Agent::start(crate::agent::AgentParams {
            cfg: config::Config::default(),
            workspace: temp_dir("verdict"),
            def,
            agents: BTreeMap::new(),
            provider: config::Provider::default(),
            instance_id: "v".into(),
            parent: None,
            parent_agent: None,
            depth: 0,
            resume: false,
            task: String::new(),
            control: crate::socket::Control::new(None, context.clone()),
            context,
        })
        .unwrap()
    }

    #[test]
    fn ask_options_accept_array_or_encoded_array_and_explain_otherwise() {
        let parse = |o: Value| {
            parse_args::<AskArgs>(&json!({"question": "q", "recommended": "a", "options": o}))
        };
        assert_eq!(parse(json!(["a", "b"])).unwrap().options, ["a", "b"]);
        assert_eq!(parse(json!("[\"a\",\"b\"]")).unwrap().options, ["a", "b"]);
        let err = format!(
            "{:#}",
            parse(json!("<parameter name=\"option\">x")).err().unwrap()
        );
        assert!(err.contains("JSON array of 2-6 strings"), "{err}");
    }

    #[test]
    fn a_review_pass_ends_with_a_verdict() {
        let r = reviewed_build().reviewer().unwrap();
        assert_eq!(
            (r.name.as_str(), r.worker(), r.prompt.as_str()),
            ("build:review", "build", "judge")
        );
        assert_eq!(r.tools, ["read", "write", "verdict"]);
        assert!(r.reviewer().is_none());
        let mut a = agent_for(r.clone());
        assert!(verdict(&mut a, &json!({"verdict":"done","notes":" "})).is_err());
        assert!(verdict(&mut a, &json!({"verdict":"maybe","notes":"x"})).is_err());
        verdict(&mut a, &json!({"verdict":"handoff","notes":"part 2"})).unwrap();
        let v = a.verdict.clone().unwrap();
        let next = v.next.unwrap();
        assert!(a.handed_off && v.status == "handoff");
        assert_eq!(
            (next.agent.as_str(), next.task.as_str()),
            ("build", "part 2")
        );
        assert!(
            verdict(&mut a, &json!({"verdict":"done","notes":"x"})).is_err(),
            "one verdict per run"
        );
        let mut a = agent_for(r);
        verdict(&mut a, &json!({"verdict":"reject","notes":"no test"})).unwrap();
        assert!(!a.handed_off && a.verdict.unwrap().next.is_none());
    }

    #[test]
    fn handoffs_honour_the_spawns_gate() {
        let mut def = reviewed_build();
        def.finish = vec!["done".into(), "handoff".into()];
        def.spawns = Some(vec!["explore".into()]);
        let mut a = agent_for(def.clone());
        for n in ["explore", "plan"] {
            let d = AgentDef {
                name: n.into(),
                ..def.clone()
            };
            a.agents.insert(n.into(), d);
        }
        let err = |r: Result<String>| format!("{:#}", r.unwrap_err());
        let e = err(spawn(&mut a, &json!({"agent":"plan","instructions":"x"})));
        assert!(
            e.contains("cannot spawn `plan`") && e.contains("explore"),
            "{e}"
        );
        let e = err(hand_off(&mut a, &json!({"agent":"plan","task":"x"})));
        assert!(e.contains("cannot spawn `plan`"), "{e}");
        // A `finish` handoff uses the same gate at the top level; a subagent reports to its parent.
        let finish = |parent: Option<&str>, to: &str| {
            let v = json!({"status":"handoff","summary":"s","next":{"agent":to,"task":"x"}});
            validate_finish(&a.def, &a.agents, parent, &v)
        };
        assert!(err(finish(None, "plan").map(|_| String::new())).contains("cannot spawn `plan`"));
        assert!(finish(None, "explore").is_ok() && finish(Some("plan"), "plan").is_ok());
    }

    fn e(old: &str, new: &str) -> (String, String) {
        (old.to_string(), new.to_string())
    }

    #[test]
    fn edits() {
        let edit = |src: &str, edits: &[(String, String)], all| apply_edits(src, edits, all);
        assert_eq!(
            edit("hello world", &[e("world", "there")], false).unwrap(),
            "hello there"
        );
        assert_eq!(
            edit("abcdefghi", &[e("abc", "x"), e("ghi", "y")], false).unwrap(),
            "xdefy"
        );
        assert!(edit("aa", &[e("a", "b")], false).is_err(), "not unique");
        assert_eq!(edit("aa", &[e("a", "b")], true).unwrap(), "bb");
        assert!(
            edit("abcdef", &[e("abc", "x"), e("cde", "y")], false).is_err(),
            "overlap"
        );
        assert!(edit("abc", &[e("zzz", "x")], false).is_err());
        assert!(edit("abc", &[e("", "x")], false).is_err());
        // Whitespace-insensitive line match keeps the original indentation and line ends.
        let src = "fn a() {\n    let x = 1;  \n    let y = 2;\n}\n";
        assert_eq!(
            edit(src, &[e("let x = 1;\nlet y = 2;", "let z = 3;")], false).unwrap(),
            "fn a() {\n    let z = 3;\n}\n"
        );
        assert!(
            edit("  a\n  b\n    a\n    b\n", &[e("a\nb", "c")], false).is_err(),
            "an ambiguous fuzzy match is refused"
        );
        assert!(
            edit("  a\n  b\n", &[e("a\nb", "c")], true).is_err(),
            "replace_all needs an exact match"
        );
    }

    #[test]
    fn ls_skips_git_ignored_and_hidden_entries() {
        let ws = temp_dir("ls");
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join(".gitignore"), "skip.txt\n").unwrap();
        for f in ["keep.txt", "skip.txt", "sub/inner.txt"] {
            std::fs::write(ws.join(f), "x").unwrap();
        }
        let git = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&ws)
            .status();
        let (mut lines, mut counts) = (Vec::new(), (0, 0));
        walk(&ws, 1, false, &mut lines, &mut counts, &ws);
        let text = lines.join("\n");
        assert!(
            text.contains("keep.txt") && text.contains("sub/inner.txt"),
            "{text}"
        );
        assert!(!text.contains(".gitignore"), "{text}");
        if git.is_ok_and(|s| s.success()) {
            assert!(!text.contains("skip.txt"), "{text}");
        }
    }

    #[test]
    fn small_results_are_verbatim_and_large_ones_spill() {
        let ws = temp_dir("bounded");
        assert_eq!(bounded_result(&ws, 100, "bash", "short".into()), "short");
        let out = bounded_result(&ws, 100, "bash", "x".repeat(500));
        let log = out
            .split("written to ")
            .nth(1)
            .and_then(|r| r.split(';').next())
            .unwrap_or_else(|| panic!("{out}"));
        assert!(Path::new(log).starts_with(std::env::temp_dir()), "{log}");
        assert_eq!(std::fs::read_to_string(log).unwrap().len(), 500);
    }

    #[test]
    fn specs_follow_the_agent_definition() {
        let agents = defs(&[
            (
                "lead",
                "---\ntools: read, bash, finish\nfinish: done, handoff, blocked\n---\nl",
            ),
            (
                "worker",
                "---\ntools: read, finish\nfinish: handoff, blocked\n---\nw",
            ),
        ]);
        let names = |d: &AgentDef| {
            specs(d, false)
                .iter()
                .map(|s| s["function"]["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(&agents["lead"]), ["read", "bash", "finish"]);
        assert_eq!(names(&agents["worker"]), ["read", "finish"]);
        let status = |d: &AgentDef, sub| {
            specs(d, sub)
                .into_iter()
                .find(|s| s["function"]["name"] == "finish")
                .unwrap()["function"]["parameters"]["properties"]["status"]["enum"]
                .clone()
        };
        assert_eq!(
            status(&agents["lead"], false),
            json!(["done", "handoff", "blocked"])
        );
        assert_eq!(status(&agents["lead"], true), json!(["handoff", "blocked"]));
        assert_eq!(
            status(&agents["worker"], false),
            json!(["handoff", "blocked"])
        );
        assert!(check(&agents["lead"]).is_ok());
        assert!(check(&defs(&[("x", "---\ntools: nope\n---\n")])["x"]).is_err());
    }

    #[test]
    fn finish_validation() {
        let agents = defs(&[
            (
                "lead",
                "---\ntools: read, finish\nfinish: done, handoff, blocked\n---\nl",
            ),
            (
                "worker",
                "---\ntools: read, finish\nfinish: handoff, blocked\n---\nw",
            ),
            (
                "scout",
                "---\ntools: read, finish\nfinish: handoff, blocked\n---\ns",
            ),
        ]);
        let run = |agent: &str, parent: Option<&str>, v: Value| {
            validate_finish(&agents[agent], &agents, parent, &v)
        };
        let handoff = |to: &str| json!({"status": "handoff", "summary": "s", "next": {"agent": to, "task": "t"}});
        let done = |summary: &str| json!({"status": "done", "summary": summary});
        assert!(run("lead", None, done("s")).is_ok());
        assert!(run("lead", None, done(" ")).is_err(), "empty summary");
        assert!(
            run("worker", None, done("s")).is_err(),
            "status not offered"
        );
        assert!(run("worker", None, handoff("lead")).is_ok());
        assert!(run("worker", None, json!({"status": "handoff", "summary": "s"})).is_err());
        assert!(run("worker", None, handoff("nope")).is_err());
        let next_on_done =
            json!({"status": "done", "summary": "s", "next": {"agent": "worker", "task": "t"}});
        assert!(run("lead", None, next_on_done).is_err());
        // A handoff's `next.task` is its report, so the summary may be left out.
        let bare = json!({"status": "handoff", "next": {"agent": "lead", "task": "t"}});
        assert_eq!(
            run("worker", None, bare).unwrap().summary,
            "handed off to lead"
        );
        assert!(run("scout", Some("lead"), handoff("lead")).is_ok());
        assert!(
            run("scout", Some("lead"), handoff("worker")).is_err(),
            "subagents report to the parent agent"
        );
        assert!(
            run("lead", Some("worker"), done("s")).is_err(),
            "subagents cannot declare done"
        );
    }

    #[test]
    fn child_report_is_always_a_handoff_or_blocked() {
        let end = |result: Value| {
            json!({"type": "instance_end", "status": "done", "report": "r", "result": result})
                .to_string()
        };
        let report = |log: &str, timed_out| -> Value {
            serde_json::from_str(&child_report(
                "c",
                "explore",
                &events::parse_lines(log),
                timed_out,
                1000,
            ))
            .unwrap()
        };
        let v = report(
            &end(
                json!({"status": "handoff", "summary": "s", "next": {"agent": "plan", "task": "findings"}}),
            ),
            false,
        );
        assert_eq!(
            (
                v["status"].as_str(),
                v["report"].as_str(),
                v["run"].as_str()
            ),
            (Some("handoff"), Some("findings"), Some("done"))
        );
        let no_verdict = format!(
            "{}\n{}",
            json!({"type": "assistant", "content": "I looked"}),
            end(Value::Null)
        );
        let v = report(&no_verdict, false);
        assert_eq!(v["status"], "blocked");
        assert!(v["report"].as_str().unwrap().contains("I looked"));
        let v = report("", true);
        assert_eq!(
            (v["status"].as_str(), v["run"].as_str()),
            (Some("blocked"), Some("timed_out"))
        );
    }
}
