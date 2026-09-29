use anyhow::{bail, Result};
use serde_json::Value;
use std::time::Duration;

use super::{opt_str, req_str};
use crate::agent::Agent;
use crate::proc;

/// Spawn the same executable as a subagent in a given mode. The subagent runs a
/// single mode (never cycles) and prints a final report on stdout.
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
        "--parent-session".to_string(),
        agent.session_id.clone(),
        "--instructions-file".to_string(),
        inst_path.to_string_lossy().to_string(),
        "--label".to_string(),
        task.clone(),
        "--depth".to_string(),
        (agent.depth + 1).to_string(),
        "--quiet-startup".to_string(),
        "--no-control".to_string(),
    ];

    let cap = agent.cfg.tool_result_max_bytes.saturating_mul(2).max(16384);
    let res = proc::run_capture(
        &exe.to_string_lossy(),
        &cmd_args,
        &agent.workspace,
        Duration::from_secs(agent.cfg.spawn_timeout_secs),
        None,
        cap,
    )?;
    let _ = std::fs::remove_file(&inst_path);

    let report = if res.stdout.trim().is_empty() {
        res.stderr.trim().to_string()
    } else {
        res.stdout.trim().to_string()
    };
    let mut out = format!(
        "subagent[{}] finished (exit={:?}, {}ms)\n--- report ---\n{}",
        mode,
        res.code,
        res.duration_ms,
        if report.is_empty() { "(empty report)" } else { &report }
    );
    if res.timed_out {
        out.push_str("\n[subagent timed out]");
    }
    Ok(out)
}
