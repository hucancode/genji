use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::time::Duration;

use super::{opt_str, req_str};
use crate::agent::Agent;
use crate::proc;

/// Spawn the same executable as a subagent in a given mode. The subagent runs a
/// single mode (never cycles). Agent-to-agent communication is machine form:
/// the subagent emits its JSONL event stream on stdout, we relay every event
/// into our own stream, and return those events to the model. The subagent's
/// final report is the `report` field of the `instance_end` event.
pub fn spawn(agent: &mut Agent, args: &Value) -> Result<String> {
    let mode = req_str(args, "mode")?;
    if !["plan", "build", "explore"].contains(&mode.as_str()) {
        bail!("spawn mode must be plan|build|explore (not retro)");
    }
    if agent.depth >= agent.cfg.max_subagent_depth {
        bail!(
            "subagent depth limit reached ({} >= {})",
            agent.depth,
            agent.cfg.max_subagent_depth
        );
    }
    let instructions = req_str(args, "instructions")?;
    let task = opt_str(args, "task").unwrap_or_else(|| format!("subagent:{mode}"));

    let exe = std::env::current_exe().unwrap_or_else(|_| "genji".into());
    let tmpdir = agent.workspace.join(".genji").join("tmp");
    std::fs::create_dir_all(&tmpdir)?;
    let inst_path = tmpdir.join(format!(
        "subagent-{}-{}.md",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::write(&inst_path, &instructions)?;

    let cmd_args = vec![
        mode.clone(),
        "--subagent".to_string(),
        "--parent-instance".to_string(),
        agent.instance_id.clone(),
        "--instructions-file".to_string(),
        inst_path.to_string_lossy().to_string(),
        "--label".to_string(),
        task.clone(),
        "--depth".to_string(),
        (agent.depth + 1).to_string(),
        "--quiet-startup".to_string(),
        "--no-control".to_string(),
    ]
    .into_iter()
    // Subagents inherit OCD so a build subagent can work the same tickets.
    .chain(agent.ocd.then(|| "--ocd".to_string()))
    .collect::<Vec<_>>();

    let cap = agent.cfg.tool_result_max_bytes.saturating_mul(2).max(16384);
    let res = proc::run_capture(
        &exe.to_string_lossy(),
        &cmd_args,
        &agent.workspace,
        Duration::from_secs(agent.cfg.spawn_timeout_secs),
        cap,
    )?;
    let _ = std::fs::remove_file(&inst_path);

    // Parse the child's JSONL event stream and hand its events back to the
    // model. The subagent's final report lives in its `instance_end` event. The
    // child's events are NOT relayed into our stdout stream; the user can read
    // the full trace with `genji inspect <subagent_instance>`.
    let mut parsed = 0usize;
    let mut sub_instance = String::new();
    let mut saw_end = false;
    let mut start: Option<Value> = None;
    let mut end: Option<Value> = None;
    let mut errors: Vec<Value> = Vec::new();
    let mut tools: Vec<Value> = Vec::new();
    for line in res.stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if event.get("type").is_none() {
            continue;
        }
        parsed += 1;
        if sub_instance.is_empty() {
            if let Some(s) = event.get("instance").and_then(|s| s.as_str()) {
                sub_instance = s.to_string();
            }
        }
        match event.get("type").and_then(|t| t.as_str()) {
            Some("instance_start") => start = Some(event.clone()),
            Some("instance_end") => {
                saw_end = true;
                end = Some(event.clone());
            }
            Some("error") => errors.push(event.clone()),
            Some("tool_call") | Some("tool_result") => tools.push(event.clone()),
            _ => {}
        }
    }

    // Fallback for a subagent that produced no event stream (for example an
    // older binary): synthesise an `instance_end` from the raw text so the report
    // is still discoverable in the events.
    if parsed == 0 {
        let text = if res.stdout.trim().is_empty() {
            res.stderr.trim().to_string()
        } else {
            res.stdout.trim().to_string()
        };
        end = Some(json!({
            "type": "instance_end",
            "status": if res.timed_out { "timed_out" } else { "unknown" },
            "report": text,
        }));
    } else if !saw_end {
        errors.push(json!({
            "type": "error",
            "message": "subagent event stream ended without instance_end",
        }));
    }
    if res.timed_out {
        if let Some(e) = end.as_mut() {
            e["status"] = json!("timed_out");
        }
        errors.push(json!({ "type": "error", "message": "subagent timed out" }));
    }

    let events = bounded_events(
        start,
        tools,
        errors,
        end,
        agent
            .cfg
            .tool_result_max_bytes
            .saturating_sub(1024)
            .max(4096),
    );
    let status = events
        .iter()
        .rev()
        .find(|e| e.get("type").and_then(|t| t.as_str()) == Some("instance_end"))
        .and_then(|e| e.get("status"))
        .and_then(|s| s.as_str())
        .unwrap_or("incomplete")
        .to_string();

    Ok(json!({
        "subagent_instance": if sub_instance.is_empty() {
            Value::Null
        } else {
            json!(sub_instance)
        },
        "mode": mode,
        "status": status,
        "exit_code": res.code,
        "timed_out": res.timed_out,
        "duration_ms": res.duration_ms as u64,
        // The report is the `report` field of the `instance_end` event.
        "events": events,
    })
    .to_string())
}

/// Assemble the subagent events handed to the parent model, bounded so the
/// tool-result cap can never cut off the trailing `instance_end` (and therefore
/// the report). `instance_start`, `error`, and `instance_end` survive; tool detail
/// is clipped and then dropped as needed.
fn bounded_events(
    start: Option<Value>,
    tools: Vec<Value>,
    errors: Vec<Value>,
    end: Option<Value>,
    budget: usize,
) -> Vec<Value> {
    let end = end.unwrap_or_else(|| json!({ "type": "instance_end", "status": "incomplete" }));
    let mut events: Vec<Value> = Vec::new();
    if let Some(s) = start {
        events.push(s);
    }
    events.extend(tools);
    events.extend(errors);
    events.push(end);

    // Clip bulky string fields.
    const CLIP: usize = 2000;
    for ev in events.iter_mut() {
        for field in ["result", "content", "reasoning", "summary", "message"] {
            if let Some(s) = ev.get(field).and_then(|v| v.as_str()) {
                if s.len() > CLIP {
                    ev[field] = json!(crate::llm::truncate(s.to_string(), CLIP));
                }
            }
        }
    }

    let size = |evs: &[Value]| {
        serde_json::to_string(evs)
            .map(|s| s.len())
            .unwrap_or(usize::MAX)
    };
    // Drop tool detail (oldest first) while keeping start/errors/end.
    while size(&events) > budget {
        let idx = events.iter().position(|e| {
            matches!(
                e.get("type").and_then(|t| t.as_str()),
                Some("tool_call") | Some("tool_result")
            )
        });
        match idx {
            Some(i) => {
                events.remove(i);
            }
            None => break,
        }
    }
    // Last resort: shrink the report so `instance_end` still fits.
    while size(&events) > budget {
        let Some(i) = events
            .iter()
            .rposition(|e| e.get("type").and_then(|t| t.as_str()) == Some("instance_end"))
        else {
            break;
        };
        let report_len = events[i]
            .get("report")
            .and_then(|v| v.as_str())
            .map(|s| s.len())
            .unwrap_or(0);
        if report_len <= 64 {
            break;
        }
        let current = events[i]
            .get("report")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let shrunk = crate::llm::truncate(current, report_len / 2);
        events[i]["report"] = json!(shrunk);
    }
    // Absolute last resort: keep only the report-bearing `instance_end`.
    if size(&events) > budget {
        if let Some(i) = events
            .iter()
            .rposition(|e| e.get("type").and_then(|t| t.as_str()) == Some("instance_end"))
        {
            let end = events.remove(i);
            events.clear();
            events.push(end);
        }
    }
    events
}
