use anyhow::{Context, Result, bail};
use std::collections::{HashMap, HashSet};
use std::io::BufRead;
#[cfg(feature = "socket")]
use std::io::{BufReader, Write};
#[cfg(feature = "socket")]
use std::os::unix::fs::PermissionsExt;
#[cfg(feature = "socket")]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
#[cfg(feature = "socket")]
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::storage::context::ContextComposer;

/// Lines buffered per watcher before further output is dropped for it.
#[cfg(feature = "socket")]
const WATCH_BACKLOG: usize = 256;

#[cfg(feature = "socket")]
const HELP: &str = "<text> queues an instruction | /watch follows events | /status | /context (JSON) | /answer <callId> <text> | /stop | /ping | /help";

/// The control of a running agent: queued instructions, answers to `ask` calls, a stop
/// request and a status line, fed from stdin (`read_commands`) and, with the `socket`
/// feature, from a Unix socket for humans.
///
/// The socket lets a person inject instructions and watch the agent work.
/// Connections are long-lived. Each line is a command and gets one reply line;
/// `/watch` additionally streams events as readable text until the client disconnects.
/// Machines use stdout (the JSONL event stream, also in the session file) and stdin
/// (`read_commands`).
pub struct Control {
    /// The socket's path; `None` when nothing listens.
    pub path: Option<PathBuf>,
    state: Mutex<State>,
    /// Signalled when an instruction is queued, an answer arrives or a stop is requested.
    wake: Condvar,
    #[cfg_attr(not(feature = "socket"), allow(dead_code))]
    context: Arc<RwLock<ContextComposer>>,
    #[cfg(feature = "socket")]
    watchers: Mutex<Vec<SyncSender<String>>>,
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
    /// A control that does not listen: its queue, answers and stop work.
    pub fn new(path: Option<PathBuf>, context: Arc<RwLock<ContextComposer>>) -> Arc<Self> {
        Arc::new(Control {
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
            #[cfg(feature = "socket")]
            watchers: Mutex::new(Vec::new()),
        })
    }

    /// A control that listens on `path`, if given and this build has the `socket` feature.
    pub fn open(path: Option<PathBuf>, context: Arc<RwLock<ContextComposer>>) -> Result<Arc<Self>> {
        let ctrl = Self::new(path, context);
        #[cfg(feature = "socket")]
        if ctrl.path.is_some() {
            ctrl.listen()?;
        }
        Ok(ctrl)
    }

    #[cfg(feature = "socket")]
    fn listen(self: &Arc<Self>) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        if path.exists() {
            // A live listener means another agent is already running here; a
            // refused connection means the socket is stale and can be removed.
            match UnixStream::connect(path) {
                Ok(_) => bail!(
                    "another genji process is already listening on {}",
                    path.display()
                ),
                Err(_) => {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        let listener = UnixListener::bind(path)
            .with_context(|| format!("binding control socket {}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("securing control socket {}", path.display()))?;
        // Blocks in `accept` for the life of the process; `shutdown` removes the
        // socket file and process exit ends the threads.
        let c = self.clone();
        thread::Builder::new()
            .name("genji-control".into())
            .spawn(move || {
                for stream in listener.incoming().flatten() {
                    let c = c.clone();
                    thread::spawn(move || {
                        let _ = serve(stream, &c);
                    });
                }
            })?;
        Ok(())
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
        if let Some(path) = &self.path {
            let _ = std::fs::remove_file(path);
        }
    }

    #[cfg(feature = "socket")]
    /// Send a line to every watcher. A watcher that is not keeping up misses lines;
    /// one that has gone is forgotten. Never blocks.
    pub fn publish(&self, line: &str) {
        self.watchers.lock().unwrap().retain(|tx| {
            !matches!(
                tx.try_send(line.to_string()),
                Err(TrySendError::Disconnected(_))
            )
        });
    }

    #[cfg(feature = "socket")]
    /// Stream published lines to `out` until it fails.
    fn watch(&self, out: Arc<Mutex<UnixStream>>) {
        let (tx, rx) = sync_channel::<String>(WATCH_BACKLOG);
        self.watchers.lock().unwrap().push(tx);
        thread::spawn(move || {
            for line in rx {
                let mut w = out.lock().unwrap();
                if writeln!(w, "{line}").and_then(|()| w.flush()).is_err() {
                    break;
                }
            }
        });
    }

    #[cfg(feature = "socket")]
    /// Run one command line and return its reply.
    fn command(&self, line: &str) -> String {
        match line {
            "/help" => HELP.to_string(),
            "/status" => format!("status: {}", self.state().status),
            "/context" => serde_json::to_string(&self.context.read().unwrap().snapshot())
                .unwrap_or_else(|e| format!("error: {e}")),
            "/stop" => {
                self.request_stop();
                "stopping".to_string()
            }
            "/ping" => "pong".to_string(),
            _ if line.starts_with("/answer") => self.answer(line["/answer".len()..].trim()),
            _ if line.starts_with('/') => {
                let cmd = line.split_whitespace().next().unwrap_or(line);
                format!("error: unknown command {cmd} (try /help)")
            }
            _ => format!("queued ({} pending)", self.queue(line)),
        }
    }

    /// Queue a user instruction; returns how many are pending.
    fn queue(&self, text: &str) -> usize {
        let mut st = self.state();
        st.queue.push(text.to_string());
        self.wake.notify_all();
        st.queue.len()
    }

    fn request_stop(&self) {
        self.state().stop = true;
        self.wake.notify_all();
    }

    /// Hand `value` to the `ask` call `id` if it is waiting.
    fn give_answer(&self, id: &str, value: String) -> bool {
        let mut st = self.state();
        let waiting = st.waiting.contains(id);
        if waiting {
            st.answers.insert(id.to_string(), value);
            self.wake.notify_all();
        }
        waiting
    }

    /// Apply the JSONL commands of `input` (the machine interface, normally stdin), one per
    /// line, in the background: `{"type":"instruction","text":…}`,
    /// `{"type":"answer","id":…,"text":…}` and `{"type":"stop"}`. A bad line is reported on
    /// stderr and skipped; the end of input changes nothing.
    pub fn read_commands(self: &Arc<Self>, input: impl BufRead + Send + 'static) {
        let c = self.clone();
        thread::spawn(move || {
            for line in input.lines().map_while(std::result::Result::ok) {
                let line = line.trim();
                if !line.is_empty()
                    && let Err(e) = c.apply(line)
                {
                    eprintln!(
                        "error: stdin command `{}`: {e:#}",
                        crate::storage::util::truncate(line, 80)
                    );
                }
            }
        });
    }

    fn apply(&self, line: &str) -> Result<()> {
        let v: serde_json::Value = serde_json::from_str(line).context("not JSON")?;
        let field = |k: &str| v[k].as_str().with_context(|| format!("missing `{k}`"));
        match v["type"].as_str() {
            Some("instruction") => {
                self.queue(field("text")?);
            }
            Some("answer") => {
                let id = field("id")?;
                if !self.give_answer(id, field("text")?.to_string()) {
                    bail!("no ask call {id} is waiting");
                }
            }
            Some("stop") => self.request_stop(),
            _ => bail!("`type` must be instruction, answer or stop"),
        }
        Ok(())
    }

    #[cfg(feature = "socket")]
    /// `<callId> <text>`: the text is a JSON string, or else taken as it is.
    fn answer(&self, args: &str) -> String {
        let Some((id, text)) = args.split_once(char::is_whitespace) else {
            return "error: usage: /answer <callId> <text>".into();
        };
        let text = text.trim();
        let value = serde_json::from_str::<String>(text).unwrap_or_else(|_| text.to_string());
        if self.give_answer(id, value) {
            "answered".to_string()
        } else {
            format!("error: no ask call {id} is waiting")
        }
    }
}

#[cfg(test)]
impl Control {
    /// Block until an `ask` call `id` is waiting for its answer.
    fn await_waiter(&self, id: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !self.state().waiting.contains(id) {
            assert!(Instant::now() < deadline, "no ask call {id} is waiting");
            thread::sleep(Duration::from_millis(1));
        }
    }
}

#[cfg(feature = "socket")]
/// One client: a command per line, a reply per command, until it disconnects.
fn serve(stream: UnixStream, c: &Control) -> Result<()> {
    let out = Arc::new(Mutex::new(stream.try_clone()?));
    let mut watching = false;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let reply = if line == "/watch" {
            if !std::mem::replace(&mut watching, true) {
                c.watch(out.clone());
            }
            "watching".to_string()
        } else {
            c.command(line)
        };
        let mut w = out.lock().unwrap();
        writeln!(w, "{reply}")?;
        w.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Control;
    use crate::storage::context::ContextComposer;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    #[test]
    fn stdin_commands_queue_answer_and_stop() {
        let ctrl = Control::new(
            None,
            Arc::new(RwLock::new(ContextComposer::new(
                String::new(),
                Vec::new(),
                1000,
            ))),
        );
        let c = ctrl.clone();
        let h = std::thread::spawn(move || c.wait_answer("c1", Duration::from_secs(5)));
        ctrl.await_waiter("c1");
        let input = concat!(
            "{\"type\":\"instruction\",\"text\":\"/not a command\"}\n",
            "garbage\n\n",
            "{\"type\":\"answer\",\"id\":\"c1\",\"text\":\"pg\"}\n",
            "{\"type\":\"stop\"}\n",
        );
        ctrl.read_commands(std::io::Cursor::new(input));
        assert_eq!(h.join().unwrap().as_deref(), Some("pg"));
        for _ in 0..50 {
            if ctrl.stop_requested() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(ctrl.stop_requested());
        assert_eq!(ctrl.drain(), ["/not a command"]);
    }
}

#[cfg(all(test, feature = "socket"))]
mod socket_tests {
    use super::Control;
    use crate::storage::context::ContextComposer;
    use crate::storage::util::temp_dir;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    use std::sync::{Arc, RwLock};
    use std::time::Duration;

    /// One command on its own connection.
    fn send(path: &Path, msg: &str) -> std::io::Result<String> {
        let mut s = UnixStream::connect(path)?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        writeln!(s, "{msg}")?;
        s.shutdown(std::net::Shutdown::Write)?;
        let mut resp = String::new();
        s.read_to_string(&mut resp)?;
        Ok(resp.trim().to_string())
    }

    fn start(tag: &str, system: &str) -> (Arc<Control>, std::path::PathBuf) {
        let sock = temp_dir(tag).join("control.sock");
        let composer = Arc::new(RwLock::new(ContextComposer::new(
            system.into(),
            Vec::new(),
            1000,
        )));
        (
            Control::open(Some(sock.clone()), composer).expect("start control"),
            sock,
        )
    }

    #[test]
    fn context_command_measures_the_shared_composer() {
        let (ctrl, sock) = start("control-ctx", "system");
        let snapshot = send(&sock, "/context").expect("send /context");
        assert!(
            snapshot.contains("\"context_window\":1000"),
            "got: {snapshot}"
        );
        assert!(snapshot.contains("\"messages\""), "got: {snapshot}");
        assert!(snapshot.contains("\"system\""), "got: {snapshot}");
        ctrl.shutdown();
    }

    #[test]
    fn answer_command_wakes_wait_answer() {
        let (ctrl, sock) = start("control-answer", "");
        assert!(
            send(&sock, "/answer call_1 \"blue\"")
                .unwrap()
                .starts_with("error")
        );
        let c = ctrl.clone();
        let h = std::thread::spawn(move || c.wait_answer("call_1", Duration::from_secs(2)));
        ctrl.await_waiter("call_1");
        assert_eq!(send(&sock, "/answer call_1 \"blue\"").unwrap(), "answered");
        assert_eq!(h.join().unwrap().as_deref(), Some("blue"));
        assert!(send(&sock, "/answer call_1").unwrap().starts_with("error"));
        assert!(ctrl.drain().is_empty());
        assert_eq!(ctrl.wait_answer("call_2", Duration::from_millis(50)), None);

        let c = ctrl.clone();
        let h = std::thread::spawn(move || c.wait_answer("call_3", Duration::from_secs(5)));
        ctrl.await_waiter("call_3");
        send(&sock, "/stop").unwrap();
        assert_eq!(h.join().unwrap(), None);
        ctrl.shutdown();
    }

    #[test]
    fn answer_accepts_bare_text() {
        let (ctrl, sock) = start("control-bare", "");
        let c = ctrl.clone();
        let h = std::thread::spawn(move || c.wait_answer("c9", Duration::from_secs(2)));
        ctrl.await_waiter("c9");
        assert_eq!(send(&sock, "/answer c9 Postgres 16").unwrap(), "answered");
        assert_eq!(h.join().unwrap().as_deref(), Some("Postgres 16"));
        ctrl.shutdown();
    }

    #[test]
    fn one_connection_carries_many_commands_and_rejects_unknown_ones() {
        let (ctrl, sock) = start("control-many", "");
        let s = UnixStream::connect(&sock).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        let mut ask = |cmd: &str| {
            writeln!(w, "{cmd}").unwrap();
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            l.trim().to_string()
        };
        assert_eq!(ask("/ping"), "pong");
        assert!(ask("/help").contains("/watch"));
        assert_eq!(ask("/ping"), "pong", "/help is a single reply line");
        assert_eq!(ask("focus on the parser"), "queued (1 pending)");
        assert!(ask("/context stats").starts_with("error: unknown command /context"));
        assert!(ask("/nope").starts_with("error: unknown command /nope"));
        assert_eq!(ctrl.drain(), ["focus on the parser"]);
        ctrl.shutdown();
    }

    #[test]
    fn status_replies_with_the_current_status_line() {
        let (ctrl, sock) = start("control-status", "");
        assert_eq!(send(&sock, "/status").unwrap(), "status: starting");
        ctrl.set_status("idle");
        assert_eq!(send(&sock, "/status").unwrap(), "status: idle");
        ctrl.shutdown();
    }

    #[test]
    fn socket_is_private_replaces_a_stale_one_and_is_removed_on_shutdown() {
        use std::os::unix::fs::PermissionsExt;
        let sock = temp_dir("control-stale").join("control.sock");
        drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
        assert!(sock.exists(), "a crashed run leaves its socket file");
        let composer = || {
            Arc::new(RwLock::new(ContextComposer::new(
                String::new(),
                Vec::new(),
                1000,
            )))
        };
        let ctrl = Control::open(Some(sock.clone()), composer()).expect("replace stale socket");
        let mode = std::fs::metadata(&sock).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(send(&sock, "/ping").unwrap(), "pong");
        assert!(
            Control::open(Some(sock.clone()), composer()).is_err(),
            "a live socket is kept"
        );
        ctrl.shutdown();
        assert!(!sock.exists());
    }

    #[test]
    fn watch_streams_published_lines_on_the_same_connection() {
        let (ctrl, sock) = start("control-watch", "");
        let s = UnixStream::connect(&sock).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut w = s.try_clone().unwrap();
        let mut r = BufReader::new(s);
        writeln!(w, "/watch").unwrap();
        let mut l = String::new();
        r.read_line(&mut l).unwrap();
        assert_eq!(l.trim(), "watching");
        ctrl.publish("hello");
        l.clear();
        r.read_line(&mut l).unwrap();
        assert_eq!(l.trim(), "hello");
        writeln!(w, "/ping").unwrap();
        l.clear();
        r.read_line(&mut l).unwrap();
        assert_eq!(l.trim(), "pong");
        ctrl.shutdown();
    }
}
