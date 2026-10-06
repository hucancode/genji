use anyhow::{Context, Result};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::storage::util::{TempPath, tmp_file};

#[derive(Debug)]
pub struct ProcResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Read a file, keeping the head and the tail when it exceeds `cap` bytes:
/// build and test failures usually sit at the end.
fn read_capped(path: &Path, cap: usize) -> String {
    let Ok(mut f) = File::open(path) else {
        return String::new();
    };
    let len = f.metadata().map_or(0, |m| m.len());
    let half = (cap / 2) as u64;
    let mut head = Vec::new();
    let mut tail = Vec::new();
    if len <= cap as u64 {
        let _ = f.read_to_end(&mut head);
    } else {
        let _ = (&mut f).take(half).read_to_end(&mut head);
        let _ = f.seek(SeekFrom::Start(len - half));
        let _ = f.read_to_end(&mut tail);
    }
    let head = String::from_utf8_lossy(&head);
    if tail.is_empty() {
        head.into_owned()
    } else {
        crate::storage::util::join_head_tail(
            &head,
            &String::from_utf8_lossy(&tail),
            (len - 2 * half) as usize,
        )
    }
}

/// Whether a process with this pid exists.
pub fn alive(pid: u32) -> bool {
    Command::new("bash")
        .args(["-c", &format!("kill -0 {pid}")])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// SIGKILL the process group led by `pid`.
pub fn kill_group(pid: u32) {
    // Through bash: its builtin `kill` takes a negative pid as a process group everywhere.
    let _ = Command::new("bash")
        .args(["-c", &format!("kill -KILL -- -{pid}")])
        .stderr(Stdio::null())
        .status();
}

/// Run `program`, capturing at most `max_read_bytes` of each output stream.
pub fn run_capture(
    program: &str,
    args: &[String],
    cwd: &Path,
    tmpdir: &Path,
    timeout: Duration,
    max_read_bytes: usize,
) -> Result<ProcResult> {
    std::fs::create_dir_all(tmpdir).ok();
    let out = TempPath(tmp_file(tmpdir, ".out", "tmp"));
    let err = TempPath(tmp_file(tmpdir, ".err", "tmp"));
    let out_file = File::create(&out.0).context("creating stdout temp")?;
    let err_file = File::create(&err.0).context("creating stderr temp")?;

    let child = Command::new(program)
        .process_group(0)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_file))
        .stderr(Stdio::from(err_file))
        .spawn()
        .with_context(|| format!("spawning `{program}`"))?;
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut child = child;
        let _ = tx.send(child.wait());
    });
    let (code, timed_out) = match rx.recv_timeout(timeout) {
        Ok(status) => (status?.code(), false),
        Err(_) => {
            // Kill the whole group so grandchildren do not outlive the timeout.
            kill_group(pid);
            let _ = rx.recv_timeout(Duration::from_secs(5));
            (None, true)
        }
    };

    Ok(ProcResult {
        code,
        stdout: read_capped(&out.0, max_read_bytes),
        stderr: read_capped(&err.0, max_read_bytes),
        timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::run_capture;
    use std::time::Duration;

    fn run(script: &str, timeout: u64, cap: usize) -> super::ProcResult {
        let ws = crate::storage::util::temp_dir("proc");
        run_capture(
            "bash",
            &["-c".into(), script.into()],
            &ws,
            &ws,
            Duration::from_secs(timeout),
            cap,
        )
        .unwrap()
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let r = run("seq 1 5000", 10, 200);
        assert!(r.stdout.starts_with("1\n2\n"));
        assert!(r.stdout.trim_end().ends_with("5000"));
        assert!(r.stdout.contains("bytes omitted"));
    }

    #[test]
    fn timeout_kills_grandchildren() {
        let r = run("sleep 4242 & wait", 1, 1000);
        assert!(r.timed_out);
        let alive = std::process::Command::new("pgrep")
            .args(["-f", "sleep 4242"])
            .status()
            .unwrap()
            .success();
        assert!(!alive);
    }
}
