use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::storage::context::ContextComposer;

/// A control socket that lets a user inject instructions into a running agent.
///
/// Protocol: one newline-terminated line per connection:
///
/// - `/answer <callId> <json string>` -> answers the pending `ask` tool call `callId`
/// - any other text -> queued as a user instruction
/// - `/status` -> returns the agent's current status
/// - `/context` -> returns the live context snapshot (messages + tools)
/// - `/stop` -> requests a graceful stop
/// - `/ping` -> liveness check
///
/// Each command receives one response line and the server then closes its
/// write side. The append-only event trace remains the complete event record.
pub struct Control {
    pub path: PathBuf,
    state: Mutex<State>,
    /// Signalled when an instruction is queued, an answer arrives or a stop is requested.
    wake: Condvar,
    context: Arc<RwLock<ContextComposer>>,
}

struct State {
    queue: Vec<String>,
    stop: bool,
    status: String,
    /// Answers to `ask` calls, by tool call id, until taken by `wait_answer`.
    answers: HashMap<String, String>,
    /// Ids of the `ask` calls currently blocked in `wait_answer`.
    waiting: HashSet<String>,
}

impl Control {
    pub fn start(path: PathBuf, context: Arc<RwLock<ContextComposer>>) -> Result<Arc<Self>> {
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
            state: Mutex::new(State {
                queue: Vec::new(),
                stop: false,
                status: "starting".to_string(),
                answers: HashMap::new(),
                waiting: HashSet::new(),
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

    /// Whether instructions are queued.
    pub fn pending(&self) -> bool {
        !self.state().queue.is_empty()
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

    /// Block until an answer for `id` arrives (`Some`), the deadline passes or a
    /// stop is requested (`None`).
    pub fn wait_answer(&self, id: &str, timeout: Duration) -> Option<String> {
        let deadline = Instant::now() + timeout;
        let mut st = self.state();
        st.waiting.insert(id.to_string());
        let answer = loop {
            if let Some(a) = st.answers.remove(id) {
                break Some(a);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if st.stop || left.is_zero() {
                break None;
            }
            st = self.wake.wait_timeout(st, left).unwrap().0;
        };
        st.waiting.remove(id);
        answer
    }

    pub fn stop_requested(&self) -> bool {
        self.state().stop
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
        _ if line.starts_with("/answer ") => {
            let parsed = line["/answer ".len()..].trim_start().split_once(' ').and_then(|(id, v)| {
                serde_json::from_str::<String>(v.trim())
                    .ok()
                    .map(|v| (id.to_string(), v))
            });
            match parsed {
                Some((id, v)) => {
                    let mut st = c.state();
                    if st.waiting.contains(&id) {
                        st.answers.insert(id, v);
                        c.wake.notify_all();
                        "answered".to_string()
                    } else {
                        format!("error: no ask call {id} is waiting")
                    }
                }
                None => "error: usage: /answer <callId> <json string>".to_string(),
            }
        }
        _ => {
            let mut st = c.state();
            st.queue.push(line.to_string());
            c.wake.notify_all();
            eprintln!(
                "[control] received user instruction ({} pending)",
                st.queue.len()
            );
            format!("queued ({} pending)", st.queue.len())
        }
    };
    let _ = stream.write_all(format!("{resp}\n").as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
    Ok(())
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
    use super::{Control, send};
    use crate::storage::context::ContextComposer;
    use crate::storage::util::temp_dir;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    #[test]
    fn context_command_measures_the_shared_composer() {
        let dir = temp_dir("control-ctx");
        let sock = dir.join("control.sock");
        let composer = Arc::new(RwLock::new(ContextComposer::new(
            "system".to_string(),
            Vec::new(),
            1000,
        )));
        let ctrl = Control::start(sock.clone(), composer).expect("start control");

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

    #[test]
    fn answer_command_wakes_wait_answer() {
        let dir = temp_dir("control-answer");
        let sock = dir.join("control.sock");
        let composer = Arc::new(RwLock::new(ContextComposer::new(String::new(), Vec::new(), 1000)));
        let ctrl = Control::start(sock.clone(), composer).expect("start control");

        assert!(send(&sock, "/answer call_1 \"blue\"").unwrap().starts_with("error"));
        let c = ctrl.clone();
        let h = std::thread::spawn(move || c.wait_answer("call_1", Duration::from_secs(2)));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(send(&sock, "/answer call_1 \"blue\"").unwrap(), "answered");
        assert_eq!(h.join().unwrap().as_deref(), Some("blue"));
        assert!(send(&sock, "/answer call_1 not-json").unwrap().starts_with("error"));
        assert!(ctrl.drain().is_empty());
        assert_eq!(ctrl.wait_answer("call_2", Duration::from_millis(50)), None);

        let c = ctrl.clone();
        let h = std::thread::spawn(move || c.wait_answer("call_3", Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(100));
        send(&sock, "/stop").unwrap();
        assert_eq!(h.join().unwrap(), None);

        ctrl.shutdown();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
