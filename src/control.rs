use anyhow::{bail, Context, Result};
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// A control socket that lets a user inject instructions into a running agent.
///
/// Protocol: one newline-terminated line per connection:
///
/// - any other text -> queued as a user instruction
/// - `/status` -> returns the agent's current status
/// - `/stop` -> requests a graceful stop
/// - `/ping` -> liveness check
///
/// The server replies with one line and closes its write side.
pub struct Control {
    pub path: PathBuf,
    queue: Mutex<VecDeque<String>>,
    stop: AtomicBool,
    shutdown: AtomicBool,
    status: Mutex<String>,
}

/// Result of polling the control socket at a safe point in the run loop.
#[derive(Debug, Default, Clone, Copy)]
pub struct ControlPoll {
    pub stop: bool,
    pub injected: usize,
}

impl Control {
    pub fn start(path: PathBuf) -> Result<Arc<Self>> {
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
        listener.set_nonblocking(true)?;
        let ctrl = Arc::new(Control {
            path: path.clone(),
            queue: Mutex::new(VecDeque::new()),
            stop: AtomicBool::new(false),
            shutdown: AtomicBool::new(false),
            status: Mutex::new("starting".to_string()),
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
                            let _ = handle(stream, &c);
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

    pub fn set_status(&self, s: impl Into<String>) {
        *self.status.lock().unwrap() = s.into();
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
    } else if line == "/stop" {
        c.stop.store(true, Ordering::SeqCst);
        "stopping".to_string()
    } else if line == "/ping" {
        "pong".to_string()
    } else {
        let mut q = c.queue.lock().unwrap();
        q.push_back(line.to_string());
        format!("queued ({} pending)", q.len())
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
