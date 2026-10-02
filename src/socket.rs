use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::thread;
use std::time::Duration;

use crate::storage::context::ContextComposer;
use crate::storage::util::valid_slug;

/// A control socket that lets a user inject instructions into a running agent.
///
/// Protocol: one newline-terminated line per connection:
///
/// - any other text -> queued as a user instruction
/// - `/status` -> returns the agent's current status
/// - `/context` -> returns the live context snapshot (messages + tools)
/// - `/stop` -> requests a graceful stop
/// - `/setplan <slug>` -> follow/refine the plan `plans_dir/<slug>.md` (`off` clears)
/// - `/ping` -> liveness check
///
/// Each command receives one response line and the server then closes its
/// write side. The append-only event trace remains the complete event record.
pub struct Control {
    pub path: PathBuf,
    plans_dir: PathBuf,
    state: Mutex<State>,
    /// Signalled when an instruction is queued or a stop is requested.
    wake: Condvar,
    context: Arc<RwLock<ContextComposer>>,
}

struct State {
    queue: Vec<String>,
    stop: bool,
    status: String,
    plan: Option<String>,
}

/// Result of polling the control socket at a safe point in the run loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct ControlPoll {
    pub stop: bool,
    pub injected: bool,
}

impl Control {
    pub fn start(
        path: PathBuf,
        plans_dir: PathBuf,
        context: Arc<RwLock<ContextComposer>>,
    ) -> Result<Arc<Self>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        if path.exists() {
            // A live listener means another agent is already running here; a
            // refused connection means the socket is stale and can be removed.
            match UnixStream::connect(&path) {
                Ok(_) => bail!(
                    "another genji process is already listening on {}",
                    path.display()
                ),
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding control socket {}", path.display()))?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing control socket {}", path.display()))?;
        let ctrl = Arc::new(Control {
            path,
            plans_dir,
            state: Mutex::new(State {
                queue: Vec::new(),
                stop: false,
                status: "starting".to_string(),
                plan: None,
            }),
            wake: Condvar::new(),
            context,
        });
        // Blocks in `accept` for the life of the process; `shutdown` removes the
        // socket file and process exit ends the thread.
        let c = ctrl.clone();
        thread::Builder::new()
            .name("genji-control".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let _ = handle(stream, &c);
                }
            })?;
        Ok(ctrl)
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap()
    }

    /// Take all queued instructions (FIFO).
    pub fn drain(&self) -> Vec<String> {
        std::mem::take(&mut self.state().queue)
    }

    /// Block until an instruction is queued (returned) or a stop is requested (`None`).
    pub fn wait_for_instruction(&self) -> Option<Vec<String>> {
        let mut st = self
            .wake
            .wait_while(self.state(), |st| st.queue.is_empty() && !st.stop)
            .unwrap();
        if st.stop {
            return None;
        }
        Some(std::mem::take(&mut st.queue))
    }

    pub fn stop_requested(&self) -> bool {
        self.state().stop
    }

    /// The plan slug selected with `/setplan`, if any. The agent polls this so a
    /// user can point a running run at a specific plan without restarting it.
    pub fn active_plan(&self) -> Option<String> {
        self.state().plan.clone()
    }

    pub fn set_status(&self, s: impl Into<String>) {
        self.state().status = s.into();
    }

    pub fn shutdown(&self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn handle(mut stream: UnixStream, c: &Control) -> Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let line = line.trim();
    let resp = match line {
        "" => "error: empty command".to_string(),
        "/status" => format!("status: {}", c.state().status),
        "/context" => serde_json::to_string(&c.context.read().unwrap().snapshot())
            .unwrap_or_else(|e| format!("error: {e}")),
        "/stop" => {
            c.state().stop = true;
            c.wake.notify_all();
            "stopping".to_string()
        }
        "/ping" => "pong".to_string(),
        "/setplan" => set_plan_command(c, ""),
        _ => match line.strip_prefix("/setplan ") {
            Some(slug) => set_plan_command(c, slug.trim()),
            None => {
                let mut st = c.state();
                st.queue.push(line.to_string());
                c.wake.notify_all();
                eprintln!(
                    "[control] received user instruction ({} pending)",
                    st.queue.len()
                );
                format!("queued ({} pending)", st.queue.len())
            }
        },
    };
    let _ = stream.write_all(format!("{resp}\n").as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
}

fn set_plan_command(c: &Control, slug: &str) -> String {
    if slug.is_empty() {
        return "error: usage: /setplan <slug>".to_string();
    }
    if slug.eq_ignore_ascii_case("off") {
        c.state().plan = None;
        return "plan cleared".to_string();
    }
    if !valid_slug(slug) {
        return format!("error: invalid plan slug `{slug}` (use letters, digits, '-' or '_')");
    }
    let file = c.plans_dir.join(format!("{slug}.md"));
    let mut st = c.state();
    st.plan = Some(slug.to_string());
    if let Some(instruction) = plan_instruction(&file, slug) {
        st.queue.push(instruction);
        c.wake.notify_all();
        format!(
            "plan set to {slug} ({} pending, existing plan queued)",
            st.queue.len()
        )
    } else {
        format!("plan set to {slug} (empty; awaiting plan content)")
    }
}

/// Build the instruction queued when `/setplan` selects a plan. Returns `None`
/// when the plan file is missing or empty, in which case the selection only
/// takes effect via the system prompt and `plan_write` fills the file later.
fn plan_instruction(plan_file: &Path, slug: &str) -> Option<String> {
    let existing = std::fs::read_to_string(plan_file).ok()?;
    if existing.trim().is_empty() {
        return None;
    }
    Some(format!(
        "Read the active plan `{slug}` at `{}` first and treat it as the source of \
         truth for this work. Update the plan if the approach changes, then continue \
         until it is satisfied.",
        plan_file.display()
    ))
}

/// Client: connect to a running agent's control socket and send one line.
pub fn send(path: &Path, msg: &str) -> Result<String> {
    let mut stream = UnixStream::connect(path).with_context(|| {
        format!(
            "connecting to control socket {} (is genji running in this workspace?)",
            path.display()
        )
    })?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    stream.write_all(msg.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut resp = String::new();
    let _ = stream.read_to_string(&mut resp);
    Ok(resp.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::{Control, plan_instruction, send};
    use crate::storage::context::ContextComposer;
    use crate::storage::util::temp_dir;
    use std::sync::{Arc, RwLock};

    fn temp_file(tag: &str, content: &str) -> std::path::PathBuf {
        let path = temp_dir(&format!("control-{tag}")).join("plan.md");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn existing_nonempty_plan_queues_instruction() {
        let file = temp_file("plan", "# Plan\n\nDo the thing.");
        let ins = plan_instruction(&file, "do-the-thing").expect("expected instruction");
        assert!(ins.contains("do-the-thing"));
        assert!(ins.contains(&file.display().to_string()));
        let _ = std::fs::remove_dir_all(file.parent().unwrap());
    }

    #[test]
    fn empty_or_missing_plan_queues_nothing() {
        let empty = temp_file("empty", "   \n\t");
        assert!(plan_instruction(&empty, "empty").is_none());
        let _ = std::fs::remove_dir_all(empty.parent().unwrap());
        assert!(
            plan_instruction(std::path::Path::new("/nonexistent/genji-plan.md"), "x").is_none()
        );
    }

    #[test]
    fn context_command_measures_the_shared_composer() {
        let dir = temp_dir("control-ctx");
        let sock = dir.join("control.sock");
        let composer = Arc::new(RwLock::new(ContextComposer::new(
            "system".to_string(),
            Vec::new(),
            1000,
        )));
        let ctrl = Control::start(sock.clone(), dir.clone(), composer).expect("start control");

        let snapshot = send(&sock, "/context").expect("send /context");
        assert!(
            snapshot.contains("\"context_window\":1000"),
            "got: {snapshot}"
        );
        assert!(snapshot.contains("\"messages\""), "got: {snapshot}");
        assert!(snapshot.contains("\"system\""), "got: {snapshot}");

        ctrl.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
