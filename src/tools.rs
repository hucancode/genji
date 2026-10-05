use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use crate::agent::Agent;
use crate::config::AgentDef;
use crate::storage::proc;
use crate::storage::util::{TempPath, relative_path, slugify, tmp_file, write_file};

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
        writeln!(out, "\n[showing lines {}-{end} of {total}; pass offset/limit for the rest]", start + 1)?;
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

fn find_unique(hay: &str, needle: &str) -> Result<(usize, usize)> {
    if needle.is_empty() {
        bail!("oldText must not be empty");
    }
    let mut it = hay.match_indices(needle);
    let shown = || needle.chars().take(60).collect::<String>();
    match (it.next(), it.next()) {
        (Some((i, _)), None) => Ok((i, i + needle.len())),
        (None, _) => {
            find_fuzzy(hay, needle).ok_or_else(|| anyhow!("oldText not found: {:?}", shown()))
        }
        (Some(_), Some(_)) => bail!("oldText is not unique: {:?}", shown()),
    }
}

fn apply_edits(content: &str, edits: &[(String, String)], replace_all: bool) -> Result<String> {
    if let [(old, new)] = edits
        && replace_all
    {
        if old.is_empty() || !content.contains(old.as_str()) {
            bail!("oldText is empty or not found");
        }
        return Ok(content.replace(old.as_str(), new));
    }
    let mut ranges = Vec::new();
    for (old, new) in edits {
        let (start, end) = find_unique(content, old)?;
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
fn git_ignored(dir: &Path, names: &[String], scratch: &Path) -> Vec<String> {
    let list = TempPath(tmp_file(scratch, "ls", "txt"));
    let run = || -> Result<Vec<String>> {
        write_file(&list.0, names.join("\n"))?;
        let out = std::process::Command::new("git")
            .args(["check-ignore", "--stdin"])
            .current_dir(dir)
            .stdin(std::fs::File::open(&list.0)?)
            .stderr(std::process::Stdio::null())
            .output()?;
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
    scratch: &Path,
    lines: &mut Vec<String>,
    counts: &mut (usize, usize),
    base: &Path,
) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    let mut names: Vec<String> = entries
        .iter()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.retain(|n| n != ".git" && (hidden || !n.starts_with('.')));
    let ignored = git_ignored(dir, &names, scratch);
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        if !names.contains(&name) || ignored.contains(&name) {
            continue;
        }
        let path = e.path();
        let shown = relative_path(base, &path);
        match e.metadata() {
            Ok(m) if m.is_dir() => {
                counts.0 += 1;
                lines.push(format!("d        {shown}/"));
                if depth > 0 {
                    walk(&path, depth - 1, hidden, scratch, lines, counts, base);
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
    let scratch = std::env::temp_dir();
    std::fs::create_dir_all(&scratch)?;
    walk(
        &root,
        a.max_depth.unwrap_or(0),
        a.show_hidden.unwrap_or(false),
        &scratch,
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

// --- plans and skills -----------------------------------------------------

#[derive(Deserialize)]
struct PlanArgs {
    title: String,
    content: String,
}

/// Write a plan to `docs/notes/<title-slug>.md`, adding a `# title` heading when missing.
fn plan_write(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: PlanArgs = parse_args(args)?;
    let path = agent
        .workspace
        .join("docs/notes")
        .join(format!("{}.md", slugify(&a.title, "plan")));
    let body = if a.content.trim_start().starts_with("# ") {
        a.content.clone()
    } else {
        format!("# {}\n\n{}", a.title.trim(), a.content.trim_start())
    };
    write_file(&path, body)?;
    Ok(format!("wrote plan to {}", agent.display_path(&path)))
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
            .map(|v| v.as_str().map(str::to_string).ok_or_else(|| D::Error::custom("options must be an array of strings")))
            .collect(),
        Value::String(s) => serde_json::from_str::<Vec<String>>(&s)
            .map_err(|_| D::Error::custom("options must be a JSON array of 2-6 strings, e.g. [\"A\", \"B\"], not a string")),
        _ => Err(D::Error::custom("options must be a JSON array of 2-6 strings")),
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
    if v.summary.trim().is_empty() && v.status == "handoff" {
        if let Some(n) = &v.next {
            v.summary = format!("handed off to {}", n.agent);
        }
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
        (Some(n), true) => {
            if !agents.contains_key(&n.agent) {
                bail!(
                    "unknown agent `{}`; available: {}",
                    n.agent,
                    agents.keys().cloned().collect::<Vec<_>>().join(", ")
                );
            }
            if n.task.trim().is_empty() {
                bail!("next.task must not be empty");
            }
            if let Some(p) = parent.filter(|p| *p != n.agent) {
                bail!("a subagent hands off to its parent agent `{p}`");
            }
        }
        (None, false) => {}
    }
    Ok(v)
}

fn finish(agent: &mut Agent, args: &Value) -> Result<String> {
    let v = validate_finish(
        &agent.def,
        &agent.agents,
        agent.parent_agent.as_deref(),
        args,
    )?;
    if agent.verdict.is_some() {
        bail!("finish was already called");
    }
    agent.verdict = Some(v);
    Ok("ok".into())
}

#[derive(Deserialize)]
struct HandOffArgs {
    agent: String,
    task: String,
}

/// Ends this run; genji continues with a fresh instance of `agent` working on `task`.
fn hand_off(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: HandOffArgs = parse_args(args)?;
    if agent.parent_agent.is_some() {
        bail!("a subagent reports with `finish`, not `hand_off`");
    }
    if !agent.agents.contains_key(&a.agent) {
        bail!(
            "unknown agent `{}`; available: {}",
            a.agent,
            agent.agents.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if a.task.trim().is_empty() {
        bail!("task must not be empty");
    }
    if agent.verdict.is_some() {
        bail!("finish or hand_off was already called");
    }
    agent.verdict = Some(Verdict {
        status: "handoff".into(),
        summary: format!("handed off to {}", a.agent),
        next: Some(Next {
            agent: a.agent,
            task: a.task,
        }),
    });
    agent.handed_off = true;
    Ok("ok".into())
}

// --- spawn ----------------------------------------------------------------

#[derive(Deserialize)]
struct SpawnArgs {
    agent: String,
    instructions: String,
}

/// The `spawn` tool result for a finished child: its handoff (or a `blocked`
/// report when it ended without one), built from its event lines.
fn child_report(child: &str, agent: &str, events: &str, timed_out: bool, cap: usize) -> String {
    let events: Vec<Value> = events
        .lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .collect();
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
    json!({ "subagent": child, "agent": agent, "status": status, "report": crate::llm::truncate(&report, cap), "run": run }).to_string()
}

/// Run a child genji and return its handoff. The child id is derived from the
/// call, so a resumed parent finds the child's session: a finished one is
/// delivered as is, an unfinished one is stopped and resumed.
fn spawn(agent: &mut Agent, args: &Value) -> Result<String> {
    let a: SpawnArgs = parse_args(args)?;
    if !agent.agents.contains_key(&a.agent) {
        bail!(
            "unknown agent `{}`; available: {}",
            a.agent,
            agent.agents.keys().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if agent.depth >= agent.cfg.max_subagent_depth {
        bail!(
            "subagent depth limit reached ({} >= {})",
            agent.depth,
            agent.cfg.max_subagent_depth
        );
    }
    let child = format!("{}-{}", agent.instance_id, agent.call_tag);
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
        "--quiet-startup",
        "--no-control",
    ]
    .map(String::from)
    .into();
    cmd.extend(["--sessions-dir".into(), sessions.to_string_lossy().into()]);
    if agent.cfg.token_limit > 0 {
        cmd.extend(["--token-limit".into(), agent.cfg.token_limit.to_string()]);
    }
    if let Ok(past) = std::fs::read_to_string(&session) {
        if past.contains("\"type\":\"instance_end\"") {
            return Ok(child_report(&child, &a.agent, &past, false, cap));
        }
        let pid = past
            .lines()
            .find_map(|l| serde_json::from_str::<Value>(l).ok())
            .and_then(|e| e["pid"].as_u64());
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
    // The child's stdout is a JSONL event stream; its last lines carry the handoff, so read it whole.
    let res = proc::run_capture(
        &exe.to_string_lossy(),
        &cmd,
        &agent.workspace,
        &tmp,
        Duration::from_secs(agent.cfg.spawn_timeout_secs),
        usize::MAX,
    )?;
    let events = if res.stdout.trim().is_empty() {
        &res.stderr
    } else {
        &res.stdout
    };
    Ok(child_report(&child, &a.agent, events, res.timed_out, cap))
}

// --- registry -------------------------------------------------------------

struct Tool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
}

fn tool(
    name: &'static str,
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
) -> Tool {
    Tool {
        name,
        description,
        parameters,
        handler,
    }
}

fn registry() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        vec![
            tool("read", "Read a text file with line numbers; offset is 1-indexed. Prefer this over cat/sed/head/tail in bash for reading files: page large files with offset/limit, and a repeat of unchanged content returns a one-line pointer to the earlier result instead of a second copy.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","description":"File path"},
                    "offset":{"type":"integer","description":"First line (1-indexed)"},
                    "limit":{"type":"integer","description":"Max lines to read (default 500)"}
                },
                "required":["path"]
            }), read),
            tool("write", "Create or overwrite a file, creating parent directories.", json!({
                "type":"object",
                "properties":{"path":{"type":"string"},"content":{"type":"string"}},
                "required":["path","content"]
            }), write),
            tool("edit", "Apply precise text replacements to a file. Each oldText must match uniquely.", json!({
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
            tool("ls", "List files/directories respecting .gitignore.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","description":"Directory (default .)"},
                    "max_depth":{"type":"integer","description":"Recursion depth; Default = 0 = lists only immediate children"},
                    "show_hidden":{"type":"boolean","description":"default false"}
                }
            }), ls),
            tool("bash", "Run a shell command via bash -c in the workspace. Returns exit code, stdout, stderr.", json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string"},
                    "cwd":{"type":"string","description":"Working directory (default workspace)"},
                    "timeout_secs":{"type":"integer"}
                },
                "required":["command"]
            }), bash),
            tool("plan_write", "Persist an implementation plan as markdown at docs/notes/<title-slug>.md. Reuse the same title to refine an existing plan.", json!({
                "type":"object",
                "properties":{
                    "title":{"type":"string","description":"Short plan title; drives the file name and default heading"},
                    "content":{"type":"string","description":"Plan body in markdown"}
                },
                "required":["title","content"]
            }), plan_write),
            tool("ask", "Ask the human a multiple-choice question and wait for the answer. If nobody replies in time, the recommended option is used. A free-text reply is possible and comes back marked as such.", json!({
                "type":"object",
                "properties":{
                    "question":{"type":"string"},
                    "options":{"type":"array","items":{"type":"string"},"minItems":2,"maxItems":6},
                    "recommended":{"type":"string","description":"Your pick; must be one of options"}
                },
                "required":["question","options","recommended"]
            }), ask),
            tool("spawn", "Run a subagent that works on the instructions and hands its report back as this call's result.", json!({
                "type":"object",
                "properties":{
                    "agent":{"type":"string","description":"Agent name"},
                    "instructions":{"type":"string","description":"Self-contained instructions for the subagent"}
                },
                "required":["agent","instructions"]
            }), spawn),
            tool("hand_off", "Your context is getting heavy or a batch is done and work remains: end this run and continue in a fresh instance of `agent` (it may be yourself) with an empty context. `task` must stand alone: the goal, what is done, what is left, where the state lives (branch, files, failing test), and the assumptions so far.", json!({
                "type":"object",
                "properties":{"agent":{"type":"string"},"task":{"type":"string"}},
                "required":["agent","task"]
            }), hand_off),
            tool("finish", "End your run. done: the goal is achieved and verified. handoff: your part is done and `next.agent` continues with `next.task` (self-contained: goal, what is done, what is left, where the state lives). blocked: a human must step in.", json!({
                "type":"object",
                "properties":{
                    "status":{"type":"string","enum":["done","handoff","blocked"]},
                    "summary":{"type":"string","description":"What was done, the evidence, where the state lives"},
                    "next":{"type":"object","description":"Required for handoff only","properties":{
                        "agent":{"type":"string"},"task":{"type":"string"}
                    },"required":["agent","task"]}
                },
                "required":["status","summary"]
            }), finish),
        ]
    })
}

/// Fails when an agent lists a tool that does not exist or an invalid finish status.
pub fn check(def: &AgentDef) -> Result<()> {
    if let Some(t) = def
        .tools
        .iter()
        .find(|t| !registry().iter().any(|r| r.name == t.as_str()))
    {
        bail!("unknown tool `{t}`");
    }
    if let Some(s) = def
        .finish
        .iter()
        .find(|s| !["done", "handoff", "blocked"].contains(&s.as_str()))
    {
        bail!("unknown finish status `{s}`");
    }
    Ok(())
}

/// Every tool as `{name, description, parameters}`.
pub fn list() -> Vec<Value> {
    registry()
        .iter()
        .map(|t| json!({"name": t.name, "description": t.description, "parameters": t.parameters}))
        .collect()
}

/// Tool definitions sent to the model for this agent.
pub fn specs(def: &AgentDef, subagent: bool) -> Vec<Value> {
    registry()
        .iter()
        .filter(|t| def.tools.iter().any(|n| n == t.name))
        .map(|t| {
            let mut parameters = t.parameters.clone();
            if t.name == "finish" {
                parameters["properties"]["status"]["enum"] = json!(finish_statuses(def, subagent));
            }
            json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": parameters}})
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
            .find(|t| t.name == name)
            .ok_or_else(|| anyhow!("unknown tool `{name}`"))?;
        (t.handler)(agent, args)
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

/// The first two thirds and the last third of `max` bytes, cut on char boundaries.
fn head_and_tail(s: &str, max: usize) -> std::borrow::Cow<'_, str> {
    let (mut head, mut tail) = (max * 2 / 3, max / 3);
    while head > 0 && !s.is_char_boundary(head) {
        head -= 1;
    }
    tail = s.len().saturating_sub(tail);
    while tail < s.len() && !s.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}\n[... {} bytes omitted ...]\n{}", &s[..head], tail - head, &s[tail..]).into()
}

/// Results over `max` bytes are clipped; the full text is spilled to a tmp file.
fn bounded_result(workspace: &Path, max: usize, name: &str, text: String) -> String {
    if text.len() <= max {
        return text;
    }
    // Failures and summaries come last in command output, so keep the tail as well.
    let clipped = if name == "bash" {
        head_and_tail(&text, max)
    } else {
        crate::llm::truncate(&text, max)
    };
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect();
    let path: PathBuf = tmp_file(&std::env::temp_dir(), &format!("tool-{safe}"), "log");
    match write_file(&path, &text) {
        Ok(()) => format!(
            "{clipped}\n[full result ({} bytes) written to {}; read it with the read tool]",
            text.len(),
            relative_path(workspace, &path)
        ),
        Err(_) => clipped.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn head_and_tail_keeps_both_ends() {
        let s = format!("{}MID{}", "a".repeat(100), "z".repeat(100));
        let out = super::head_and_tail(&s, 30);
        assert!(out.starts_with("aaaa") && out.ends_with("zzzz") && out.contains("omitted"));
        assert!(!out.contains("MID") && out.len() < 100);
    }

    #[test]
    fn handoff_without_summary_is_accepted() {
        let def = crate::config::AgentDef {
            name: "build".into(),
            description: String::new(),
            prompt: String::new(),
            tools: vec![],
            skills: vec![],
            context: vec![],
            finish: vec!["done".into(), "handoff".into()],
            model: None,
            internal: false,
        };
        let agents = std::collections::BTreeMap::from([("build".to_string(), def.clone())]);
        let args = serde_json::json!({"status":"handoff","next":{"agent":"build","task":"t"}});
        let v = super::validate_finish(&def, &agents, None, &args).unwrap();
        assert_eq!(v.summary, "handed off to build");
        let none = serde_json::json!({"status":"done"});
        assert!(super::validate_finish(&def, &agents, None, &none).is_err());
    }

    #[test]
    fn ask_options_accept_array_or_encoded_array_and_explain_otherwise() {
        let ok = |v: serde_json::Value| super::parse_args::<super::AskArgs>(&v);
        let base = |o: serde_json::Value| serde_json::json!({"question":"q","recommended":"a","options":o});
        assert_eq!(ok(base(serde_json::json!(["a","b"]))).unwrap().options, ["a", "b"]);
        assert_eq!(ok(base(serde_json::json!("[\"a\",\"b\"]"))).unwrap().options, ["a", "b"]);
        let err = format!("{:#}", ok(base(serde_json::json!("\n<parameter name=\"option\">x"))).err().unwrap());
        assert!(err.contains("JSON array of 2-6 strings"), "{err}");
    }

    use super::*;
    use crate::config;
    use crate::storage::util::temp_dir;

    fn e(old: &str, new: &str) -> (String, String) {
        (old.to_string(), new.to_string())
    }

    #[test]
    fn edits() {
        assert_eq!(
            apply_edits("hello world", &[e("world", "there")], false).unwrap(),
            "hello there"
        );
        assert_eq!(
            apply_edits("abcdefghi", &[e("abc", "x"), e("ghi", "y")], false).unwrap(),
            "xdefy"
        );
        assert!(apply_edits("aa", &[e("a", "b")], false).is_err());
        assert_eq!(apply_edits("aa", &[e("a", "b")], true).unwrap(), "bb");
        assert!(apply_edits("abcdef", &[e("abc", "x"), e("cde", "y")], false).is_err());
        assert!(apply_edits("abc", &[e("zzz", "x")], false).is_err());
        let src = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n";
        assert_eq!(
            apply_edits(src, &[e("let x = 1;\nlet y = 2;", "let z = 3;")], false).unwrap(),
            "fn a() {\n    let z = 3;\n}\n"
        );
        assert!(apply_edits("  a\n  b\n    a\n    b\n", &[e("a\nb", "c")], false).is_err());
    }

    #[test]
    fn small_results_are_verbatim_and_large_ones_spill() {
        let ws = temp_dir("bounded");
        assert_eq!(bounded_result(&ws, 100, "bash", "short".into()), "short");
        let out = bounded_result(&ws, 100, "bash", "x".repeat(500));
        assert!(out.contains("written to /tmp/tool-bash-"), "{out}");
        let log = out.split("written to ").nth(1).unwrap().split(';').next().unwrap();
        assert_eq!(
            std::fs::read_to_string(log)
                .unwrap()
                .len(),
            500
        );
    }

    #[test]
    fn specs_follow_the_agent_definition() {
        let ws = temp_dir("specs");
        config::tests::write_agents(
            &ws,
            &[
                ("lead", "---\ntools: read, plan_write, finish\nfinish: done, handoff, blocked\n---\nl"),
                ("worker", "---\ntools: read, finish\nfinish: handoff, blocked\n---\nw"),
            ],
        );
        let agents = config::load_agents(&ws);
        let names = |d: &AgentDef| {
            specs(d, false)
                .iter()
                .map(|s| s["function"]["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert!(names(&agents["lead"]).contains(&"plan_write".to_string()));
        assert!(!names(&agents["worker"]).contains(&"plan_write".to_string()));
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
    }

    #[test]
    fn finish_validation() {
        let ws = temp_dir("finish");
        config::tests::write_agents(
            &ws,
            &[
                ("lead", "---\ntools: read, finish\nfinish: done, handoff, blocked\n---\nl"),
                ("worker", "---\ntools: read, finish\nfinish: handoff, blocked\n---\nw"),
                ("scout", "---\ntools: read, finish\nfinish: handoff, blocked\n---\ns"),
            ],
        );
        let agents = config::load_agents(&ws);
        let run = |agent: &str, parent: Option<&str>, v: Value| {
            validate_finish(&agents[agent], &agents, parent, &v)
        };
        let handoff = |to: &str| json!({"status": "handoff", "summary": "s", "next": {"agent": to, "task": "t"}});
        assert!(run("lead", None, json!({"status": "done", "summary": "s"})).is_ok());
        assert!(
            run("worker", None, json!({"status": "done", "summary": "s"})).is_err(),
            "build cannot declare done"
        );
        assert!(run("worker", None, handoff("lead")).is_ok());
        assert!(run("worker", None, json!({"status": "handoff", "summary": "s"})).is_err());
        assert!(run("worker", None, handoff("nope")).is_err());
        assert!(
            run(
                "lead",
                None,
                json!({"status": "done", "summary": "s", "next": {"agent": "worker", "task": "t"}})
            )
            .is_err()
        );
        assert!(run("lead", None, json!({"status": "done", "summary": " "})).is_err());
        assert!(run("scout", Some("lead"), handoff("lead")).is_ok());
        assert!(
            run("scout", Some("lead"), handoff("worker")).is_err(),
            "subagents report to the parent agent"
        );
        assert!(
            run(
                "lead",
                Some("worker"),
                json!({"status": "done", "summary": "s"})
            )
            .is_err()
        );
    }

    #[test]
    fn child_report_is_always_a_handoff_or_blocked() {
        let end = |result: Value| {
            json!({"type": "instance_end", "status": "done", "report": "r", "result": result})
                .to_string()
        };
        let ok = end(
            json!({"status": "handoff", "summary": "s", "next": {"agent": "plan", "task": "findings"}}),
        );
        let v: Value =
            serde_json::from_str(&child_report("c", "explore", &ok, false, 1000)).unwrap();
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
        let v: Value =
            serde_json::from_str(&child_report("c", "explore", &no_verdict, false, 1000)).unwrap();
        assert_eq!(v["status"], "blocked");
        assert!(v["report"].as_str().unwrap().contains("I looked"));
        let v: Value = serde_json::from_str(&child_report("c", "explore", "", true, 1000)).unwrap();
        assert_eq!(
            (v["status"].as_str(), v["run"].as_str()),
            (Some("blocked"), Some("timed_out"))
        );
    }
}
