//! Killable workers bound SQLite, parsing, and filesystem work together.
//!
//! A timeout around a Rust thread would leave its transaction and locks alive.
//! Each job instead owns a child process, which is killed and reaped before a
//! timeout is reported. The parent retains the MCP connection and session stats.

use super::{MAX_MESSAGE, MAX_OUTPUT, ToolData, call_tool_detailed, ensure_targets};
use crate::repo;
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(super) struct TimedOut;

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MCP worker deadline exceeded")
    }
}

impl std::error::Error for TimedOut {}

#[derive(Serialize, Deserialize)]
struct Work {
    store: PathBuf,
    targets: Vec<repo::Target>,
    // None means startup indexing; Some means a tools/call request.
    params: Option<Value>,
    no_refresh: bool,
    timeout_ms: u64,
}

pub fn worker_main() -> Result<()> {
    let result = (|| -> Result<ToolData> {
        let mut bytes = Vec::new();
        std::io::stdin()
            .take((2 * MAX_MESSAGE + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 2 * MAX_MESSAGE, "worker input too large");
        let work: Work = serde_json::from_slice(&bytes)?;
        ensure!(
            (1..=300_000).contains(&work.timeout_ms),
            "invalid worker timeout"
        );
        // A killed/crashed MCP parent cannot run its cleanup handler. Keep an
        // independent ceiling in the child so it cannot become an orphan that
        // holds SQLite locks indefinitely. Process exit closes every connection.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(work.timeout_ms));
            std::process::exit(124);
        });
        if let Some(params) = work.params {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .context("missing tool name")?;
            call_tool_detailed(
                &work.store,
                &work.targets,
                name,
                params.get("arguments").unwrap_or(&Value::Null),
                work.no_refresh,
            )
        } else {
            ensure_targets(&work.store, &work.targets, work.no_refresh)?;
            Ok(ToolData {
                value: Value::Null,
                baseline_bytes: 0,
                baseline_files: 0,
            })
        }
    })()
    .map_err(|error| format!("{error:#}"));
    serde_json::to_writer(std::io::stdout().lock(), &result)?;
    Ok(())
}

/// Also cleans up on early I/O errors and unwinding.
struct OwnedChild(Child);

impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn execute(
    command: &mut Command,
    input: Vec<u8>,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Vec<u8>> {
    if Instant::now() >= deadline {
        return Err(TimedOut.into());
    }
    let mut child = OwnedChild(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start isolated MCP worker")?,
    );
    let mut stdin = child.0.stdin.take().context("worker stdin missing")?;
    let stdout = child.0.stdout.take().context("worker stdout missing")?;
    // Drain output concurrently: a pipe-filling result must not deadlock wait().
    // Sending input here also keeps a stuck worker's stdin inside the deadline.
    let io = std::thread::spawn(move || -> Result<Vec<u8>> {
        stdin.write_all(&input)?;
        drop(stdin);
        let mut bytes = Vec::new();
        stdout
            .take((MAX_OUTPUT + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_OUTPUT,
            "response too large; narrow the query"
        );
        Ok(bytes)
    });
    let status = loop {
        if cancelled.load(Ordering::Relaxed) {
            break Err(anyhow!("MCP indexing cancelled"));
        }
        if Instant::now() >= deadline {
            break Err(TimedOut.into());
        }
        match child.0.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => break Err(error.into()),
        }
        std::thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    };
    // Close every SQLite handle before returning the timeout to the client.
    drop(child);
    let output = io
        .join()
        .map_err(|_| anyhow!("MCP worker I/O thread panicked"));
    let status = status?;
    if status.code() == Some(124) {
        return Err(TimedOut.into());
    }
    let bytes = output??;
    ensure!(status.success(), "MCP worker exited with {status}");
    Ok(bytes)
}

pub(super) fn run(
    store: &Path,
    targets: &[repo::Target],
    params: Option<Value>,
    no_refresh: bool,
    deadline: Instant,
) -> Result<ToolData> {
    run_cancellable(
        store,
        targets,
        params,
        no_refresh,
        deadline,
        &AtomicBool::new(false),
    )
}

fn run_cancellable(
    store: &Path,
    targets: &[repo::Target],
    params: Option<Value>,
    no_refresh: bool,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<ToolData> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(TimedOut.into());
    }
    let input = serde_json::to_vec(&Work {
        store: store.to_path_buf(),
        targets: targets.to_vec(),
        params,
        no_refresh,
        timeout_ms: remaining.as_millis().clamp(1, 300_000) as u64,
    })?;
    let mut command =
        Command::new(std::env::current_exe().context("locate MCP worker executable")?);
    command.arg("__mcp-worker");
    let bytes = execute(&mut command, input, deadline, cancelled)?;
    let result: std::result::Result<ToolData, String> =
        serde_json::from_slice(&bytes).context("invalid MCP worker response")?;
    result.map_err(|problem| anyhow!(problem))
}

pub(super) struct Background {
    task: Option<JoinHandle<Result<ToolData>>>,
    cancelled: Arc<AtomicBool>,
}

impl Background {
    pub(super) fn start(store: &Path, targets: &[repo::Target], timeout: Duration) -> Self {
        let store = store.to_path_buf();
        let targets = targets.to_vec();
        let cancelled = Arc::new(AtomicBool::new(false));
        let signal = Arc::clone(&cancelled);
        let deadline = Instant::now() + timeout;
        let task = std::thread::spawn(move || {
            run_cancellable(&store, &targets, None, false, deadline, &signal)
        });
        Self {
            task: Some(task),
            cancelled,
        }
    }

    pub(super) fn finish(mut self) -> Result<()> {
        self.task
            .take()
            .expect("background worker present")
            .join()
            .map_err(|_| anyhow!("background MCP worker panicked"))??;
        Ok(())
    }
}

impl Drop for Background {
    fn drop(&mut self) {
        // EOF/client disconnect must not leave an orphan indexing process.
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(task) = self.task.take() {
            let _ = task.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, mcp::tests::TempDir};

    // Run only as a child of the timeout test, using a disposable database.
    #[test]
    #[ignore = "subprocess fixture for timeout cleanup"]
    fn sqlite_worker_fixture() {
        let root = PathBuf::from(std::env::var_os("PANOPTES_TEST_WORKER_DIR").unwrap());
        let conn = db::open(&root.join("store.db")).unwrap();
        conn.execute_batch(
            "create table probe(value integer); begin immediate; insert into probe values (1);",
        )
        .unwrap();
        std::fs::write(root.join("ready"), "ready").unwrap();
        // Deliberately expensive SQLite work while holding a write transaction.
        // The supervisor must kill the query and release that transaction.
        let _: i64 = conn.query_row(
            "with recursive n(x) as (values(0) union all select x+1 from n where x<1000000000) select sum(x) from n",
            [], |row| row.get(0),
        ).unwrap();
    }

    #[test]
    fn timeout_kills_sqlite_query_rolls_back_and_releases_write_lock() {
        let temp = TempDir::new("worker-timeout");
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "mcp::worker::tests::sqlite_worker_fixture",
                "--ignored",
            ])
            .env("PANOPTES_TEST_WORKER_DIR", &temp.0);
        let started = Instant::now();
        let error = execute(
            &mut command,
            Vec::new(),
            started + Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap_err();
        assert!(error.is::<TimedOut>(), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(8));
        assert!(
            temp.0.join("ready").exists(),
            "fixture must acquire its write lock before timing out"
        );
        let conn = db::open(&temp.0.join("store.db")).unwrap();
        conn.busy_timeout(Duration::ZERO).unwrap();
        let rows: i64 = conn
            .query_row("select count(*) from probe", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0, "the interrupted transaction must roll back");
        conn.execute("insert into probe values (2)", []).unwrap();
        let check: String = conn
            .query_row("pragma integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(check, "ok");
    }
}
