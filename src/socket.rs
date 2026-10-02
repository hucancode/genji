use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use crate::storage::context::ContextComposer;

/// A control socket that lets a user inject instructions into a running agent.
///
/// Protocol: one newline-terminated line per connection:
///
/// - any other text -> queued as a user instruction
/// - `/status` -> returns the agent's current status
/// - `/context` -> returns the live context snapshot (messages + tools)
/// - `/context stats` -> returns the computed token breakdown
/// - `/stop` -> requests a graceful stop
/// - `/setplan <slug>` -> follow/refine the plan `plans_dir/<slug>.md` (`off` clears)
/// - `/ping` -> liveness check
///
/// Each command receives one response line and the server then closes its
/// write side. The append-only event trace remains the complete event record.
pub struct Control {
    pub path: PathBuf,
    plans_dir: PathBuf,
    queue: Mutex<VecDeque<String>>,
    stop: AtomicBool,
    shutdown: AtomicBool,
    status: Mutex<String>,
    plan: Mutex<Option<String>>,
    context: Arc<RwLock<ContextComposer>>,
}

/// Result of polling the control socket at a safe point in the run loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct ControlPoll {
    pub stop: bool,
    pub injected: usize,
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
        listener.set_nonblocking(true)?;
        let ctrl = Arc::new(Control {
            path: path.clone(),
            plans_dir,
            queue: Mutex::new(VecDeque::new()),
            stop: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            status: Mutex::new("starting".to_string()),
            plan: Mutex::new(None),
            context,
        });
        let c = ctrl.clone();
        thread::Builder::new()
            .name("genji-control".into())
            .spawn(move || {
                loop {
                    if c.shutdown.load(Ordering::SeqCst) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let client = c.clone();
                            let _ = thread::Builder::new()
                                .name("genji-control-client".into())
                                .spawn(move || {
                                    let _ = handle(stream, &client);
                                });
                        }
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(50));
                        }
                        Err(_) => thread::sleep(Duration::from_millis(50)),
                    }
                }
                let _ = std::fs::remove_file(&path);
            })?;
        Ok(ctrl)
    }

    /// Take all queued instructions (FIFO).
    pub fn drain(&self) -> Vec<String> {
        let mut q = self.queue.lock().unwrap();
        q.drain(..).collect()
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// The plan slug selected with `/setplan`, if any. The agent polls this so a
    /// user can point a running run at a specific plan without restarting it.
    pub fn active_plan(&self) -> Option<String> {
        self.plan.lock().unwrap().clone()
    }

    fn set_plan(&self, slug: Option<String>) {
        *self.plan.lock().unwrap() = slug;
    }

    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
    }

    fn context_snapshot(&self) -> Value {
        self.context.read().unwrap().snapshot()
    }

    fn context_stats(&self) -> Value {
        self.context.read().unwrap().stats().to_json()
    }

    pub fn status(&self) -> String {
        self.status.lock().unwrap().clone()
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = std::fs::remove_file(&self.path);
    }
}

fn handle(mut stream: UnixStream, c: &Control) -> Result<()> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let line = line.trim();
    let resp = if line.is_empty() {
        "error: empty command".to_string()
    } else if line == "/status" {
        format!("status: {}", c.status())
    } else if line == "/context" {
        serde_json::to_string(&c.context_snapshot()).unwrap_or_else(|e| format!("error: {e}"))
    } else if line == "/context stats" {
        serde_json::to_string(&c.context_stats()).unwrap_or_else(|e| format!("error: {e}"))
    } else if line == "/stop" {
        c.stop.store(true, Ordering::SeqCst);
        "stopping".to_string()
    } else if line == "/setplan" || line == "/setplan " {
        "error: usage: /setplan <slug>".to_string()
    } else if let Some(rest) = line.strip_prefix("/setplan ") {
        let slug = rest.trim();
        if slug.is_empty() {
            "error: usage: /setplan <slug>".to_string()
        } else if slug.eq_ignore_ascii_case("off") {
            c.set_plan(None);
            "plan cleared".to_string()
        } else if !valid_plan_slug(slug) {
            format!("error: invalid plan slug `{slug}` (use letters, digits, '-' or '_')")
        } else {
            c.set_plan(Some(slug.to_string()));
            // An existing, non-empty plan is read and refined by the agent; a
            // missing or empty one is only pointed at, so `plan_write` creates
            // or fills it later without an instruction to follow yet.
            let file = c.plans_dir.join(format!("{slug}.md"));
            let mut q = c.queue.lock().unwrap();
            if let Some(instruction) = plan_instruction(&file, slug) {
                q.push_back(instruction);
                format!(
                    "plan set to {slug} ({} pending, existing plan queued)",
                    q.len()
                )
            } else {
                format!("plan set to {slug} (empty; awaiting plan content)")
            }
        }
    } else if line == "/ping" {
        "pong".to_string()
    } else {
        let mut q = c.queue.lock().unwrap();
        q.push_back(line.to_string());
        eprintln!("[control] received user instruction ({} pending)", q.len());
        format!("queued ({} pending)", q.len())
    };
    let _ = stream.write_all(format!("{resp}\n").as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
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

/// A `/setplan` slug must be safe to use as a bare file name: letters, digits,
/// `-` and `_`, at most 64 chars, and no path separators.
fn valid_plan_slug(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= 64
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
    use super::{Control, plan_instruction, send, valid_plan_slug};
    use crate::storage::context::ContextComposer;
    use std::sync::{Arc, RwLock};

    fn temp_file(tag: &str, content: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let path = std::env::temp_dir().join(format!("genji-control-{tag}-{nanos}.md"));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn existing_nonempty_plan_queues_instruction() {
        let file = temp_file("plan", "# Plan\n\nDo the thing.");
        let ins = plan_instruction(&file, "do-the-thing").expect("expected instruction");
        assert!(ins.contains("do-the-thing"));
        assert!(ins.contains(&file.display().to_string()));
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn empty_or_missing_plan_queues_nothing() {
        let empty = temp_file("empty", "   \n\t");
        assert!(plan_instruction(&empty, "empty").is_none());
        let _ = std::fs::remove_file(&empty);
        assert!(
            plan_instruction(std::path::Path::new("/nonexistent/genji-plan.md"), "x").is_none()
        );
    }

    #[test]
    fn plan_slugs_are_safe_file_names() {
        assert!(valid_plan_slug("rate-limiting"));
        assert!(valid_plan_slug("Plan_2"));
        assert!(!valid_plan_slug(""));
        assert!(!valid_plan_slug("../escape"));
        assert!(!valid_plan_slug("a/b"));
        assert!(!valid_plan_slug("has space"));
        assert!(!valid_plan_slug(&"x".repeat(65)));
    }

    #[test]
    fn context_command_measures_the_shared_composer() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("genji-control-ctx-{nanos}"));
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
        // The snapshot is a raw read; the token breakdown is opt-in.
        assert!(
            !snapshot.contains("system_prompt_tokens"),
            "got: {snapshot}"
        );

        let breakdown = send(&sock, "/context stats").expect("send /context stats");
        assert!(
            breakdown.contains("\"context_window\":1000"),
            "got: {breakdown}"
        );
        assert!(
            breakdown.contains("\"system_prompt_tokens\""),
            "got: {breakdown}"
        );

        ctrl.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
