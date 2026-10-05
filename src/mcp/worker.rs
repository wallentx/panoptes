//! Killable workers bound SQLite, parsing, and filesystem work together.
//!
//! A timeout around a Rust thread would leave its transaction and locks alive.
//! Each job instead owns a child process, which is killed and reaped before a
//! timeout is reported. The parent retains the MCP connection and session stats.

use super::{MAX_MESSAGE, MAX_OUTPUT, ToolData, call_tool_detailed};
use crate::{progress, repo};
use anyhow::{Context, Result, anyhow, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub(super) struct TimedOut;

impl std::fmt::Display for TimedOut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MCP worker inactivity limit exceeded")
    }
}

impl std::error::Error for TimedOut {}

#[derive(Clone, Copy)]
pub(super) struct Limits {
    pub deadline: Instant,
    pub idle_timeout: Duration,
}

#[derive(Serialize, Deserialize)]
struct ProgressPacket {
    progress: progress::Progress,
}

#[derive(Serialize, Deserialize)]
struct Work {
    store: PathBuf,
    targets: Vec<repo::Target>,
    params: Value,
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
        // The child also watches inactivity: a dead parent must not leave an
        // orphan holding SQLite locks. Real, flushed progress resets this timer.
        let last_progress = Arc::new(Mutex::new(Instant::now()));
        let watch_progress = last_progress.clone();
        let timeout = Duration::from_millis(work.timeout_ms);
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(timeout.min(Duration::from_millis(100)));
                if watch_progress.lock().unwrap().elapsed() >= timeout {
                    std::process::exit(124);
                }
            }
        });
        progress::install(move |progress| {
            let mut output = std::io::stdout().lock();
            let sent = serde_json::to_writer(&mut output, &ProgressPacket { progress })
                .map_err(std::io::Error::other)
                .and_then(|()| output.write_all(b"\n"))
                .and_then(|()| output.flush());
            if sent.is_err() {
                std::process::exit(1);
            }
            *last_progress.lock().unwrap() = Instant::now();
        });
        progress::report("Starting request", 0, None, "");
        let params = work.params;
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
    })()
    .map_err(|error| format!("{error:#}"));
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(&mut output, &result)?;
    output.write_all(b"\n")?;
    output.flush()?;
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
    limits: Limits,
    cancelled: &AtomicBool,
    mut report: impl FnMut(progress::Progress),
) -> Result<Vec<u8>> {
    let mut deadline = limits.deadline;
    if Instant::now() >= deadline {
        return Err(TimedOut.into());
    }
    anyhow::ensure!(!cancelled.load(Ordering::Relaxed), "MCP request cancelled");
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
    let (updates, progress_events) = mpsc::sync_channel(8);
    // Progress is framed separately from the bounded final result. Drain both
    // while the child runs, with backpressure instead of an unbounded log queue.
    let io = std::thread::spawn(move || -> Result<Vec<u8>> {
        stdin.write_all(&input)?;
        drop(stdin);
        let mut output = BufReader::new(stdout);
        let mut result = Vec::new();
        loop {
            let mut line = Vec::new();
            let read = output
                .by_ref()
                .take((MAX_OUTPUT + 1) as u64)
                .read_until(b'\n', &mut line)?;
            if read == 0 {
                break;
            }
            ensure!(line.len() <= MAX_OUTPUT, "worker message too large");
            if let Ok(packet) = serde_json::from_slice::<ProgressPacket>(&line) {
                let _ = updates.send((packet.progress, Instant::now()));
            } else {
                ensure!(
                    result.len() + line.len() <= MAX_OUTPUT,
                    "response too large; narrow the query"
                );
                result.extend(line);
            }
        }
        Ok(result)
    });
    let mut previous = None;
    let status = loop {
        while let Ok((update, observed)) = progress_events.try_recv() {
            // A repeated heartbeat/status does not buy more time.
            if previous.as_ref() != Some(&update) {
                deadline = observed + limits.idle_timeout;
                previous = Some(update.clone());
                report(update);
            }
        }
        if cancelled.load(Ordering::Relaxed) {
            break Err(anyhow!("MCP request cancelled"));
        }
        if Instant::now() >= deadline {
            break Err(TimedOut.into());
        }
        match child.0.try_wait() {
            Ok(Some(status)) if io.is_finished() => {
                while let Ok((update, _)) = progress_events.try_recv() {
                    if previous.as_ref() != Some(&update) {
                        previous = Some(update.clone());
                        report(update);
                    }
                }
                break Ok(status);
            }
            Ok(_) => {}
            Err(error) => break Err(error.into()),
        }
        std::thread::sleep(
            Duration::from_millis(10).min(deadline.saturating_duration_since(Instant::now())),
        );
    };
    // Drop the receiver before joining: a stopped supervisor must also unblock
    // the pipe reader if it was waiting to deliver a final progress update.
    drop(progress_events);
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
    params: Value,
    no_refresh: bool,
    limits: Limits,
    cancelled: &AtomicBool,
    report: impl FnMut(progress::Progress),
) -> Result<ToolData> {
    let remaining = limits.deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return Err(TimedOut.into());
    }
    let input = serde_json::to_vec(&Work {
        store: store.to_path_buf(),
        targets: targets.to_vec(),
        params,
        no_refresh,
        timeout_ms: limits.idle_timeout.as_millis().clamp(1, 300_000) as u64,
    })?;
    let mut command = crate::executable::command().context("locate MCP worker executable")?;
    command.arg("__mcp-worker");
    let bytes = execute(&mut command, input, limits, cancelled, report)?;
    let result: std::result::Result<ToolData, String> =
        serde_json::from_slice(&bytes).context("invalid MCP worker response")?;
    result.map_err(|problem| anyhow!(problem))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db, mcp::tests::TempDir};

    #[test]
    #[ignore = "subprocess fixture for inactivity supervision"]
    fn progress_worker_fixture() {
        let changing = std::env::var("PANOPTES_TEST_PROGRESS").unwrap() == "changing";
        for step in 0..8 {
            let packet = ProgressPacket {
                progress: progress::Progress {
                    stage: "Parsing files".into(),
                    completed: if changing { step } else { 0 },
                    total: Some(8),
                    detail: "fixture".into(),
                },
            };
            let mut output = std::io::stdout().lock();
            serde_json::to_writer(&mut output, &packet).unwrap();
            output.write_all(b"\n").unwrap();
            output.flush().unwrap();
            drop(output);
            std::thread::sleep(Duration::from_millis(250));
        }
    }

    fn progress_fixture(changing: bool) -> (Result<Vec<u8>>, Duration, usize) {
        let mut command = crate::executable::command().unwrap();
        command
            .args([
                "--exact",
                "mcp::worker::tests::progress_worker_fixture",
                "--ignored",
                "--nocapture",
                "--quiet",
            ])
            .env(
                "PANOPTES_TEST_PROGRESS",
                if changing { "changing" } else { "repeating" },
            );
        let started = Instant::now();
        let mut updates = 0;
        let result = execute(
            &mut command,
            Vec::new(),
            Limits {
                deadline: started + Duration::from_secs(2),
                idle_timeout: Duration::from_millis(700),
            },
            &AtomicBool::new(false),
            |_| updates += 1,
        );
        (result, started.elapsed(), updates)
    }

    #[test]
    fn meaningful_progress_extends_the_inactivity_deadline() {
        let (result, elapsed, updates) = progress_fixture(true);
        assert!(result.is_ok(), "{result:?}");
        assert!(elapsed >= Duration::from_secs(2));
        assert_eq!(updates, 8);
    }

    #[test]
    fn repeated_status_does_not_extend_the_inactivity_deadline() {
        let (result, elapsed, updates) = progress_fixture(false);
        assert!(result.unwrap_err().is::<TimedOut>());
        assert!(elapsed < Duration::from_secs(2));
        assert_eq!(updates, 1, "duplicate heartbeats are not progress");
    }

    #[test]
    #[ignore = "subprocess fixture for SQLite progress"]
    fn sqlite_progress_fixture() {
        progress::install(|update| {
            let mut output = std::io::stdout().lock();
            serde_json::to_writer(&mut output, &ProgressPacket { progress: update }).unwrap();
            output.write_all(b"\n").unwrap();
            output.flush().unwrap();
        });
        let temp = TempDir::new("sql-progress-fixture");
        let conn = db::open(&temp.0.join("store.db")).unwrap();
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(1) {
            let total: i64 = conn.query_row("with recursive n(x) as (values(0) union all select x+1 from n where x<10000) select sum(x) from n", [], |row| row.get(0)).unwrap();
            assert_eq!(total, 50_005_000);
        }
    }

    #[test]
    fn sqlite_execution_reports_real_work_and_extends_inactivity() {
        let mut command = crate::executable::command().unwrap();
        command.args([
            "--exact",
            "mcp::worker::tests::sqlite_progress_fixture",
            "--ignored",
            "--nocapture",
            "--quiet",
        ]);
        let started = Instant::now();
        let mut counters = Vec::new();
        let result = execute(
            &mut command,
            Vec::new(),
            Limits {
                deadline: started + Duration::from_secs(2),
                idle_timeout: Duration::from_millis(350),
            },
            &AtomicBool::new(false),
            |update| {
                assert_eq!(update.stage, "Processing database");
                counters.push(update.completed);
            },
        );
        assert!(result.is_ok(), "{result:?}");
        assert!(started.elapsed() >= Duration::from_secs(1));
        assert!(counters.len() > 2);
        assert!(counters.windows(2).all(|pair| pair[0] < pair[1]));
    }

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
        let mut command = crate::executable::command().unwrap();
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
            Limits {
                deadline: started + Duration::from_secs(2),
                idle_timeout: Duration::from_secs(2),
            },
            &AtomicBool::new(false),
            |_| {},
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
