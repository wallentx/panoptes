//! Responsive transport with bounded, killable query jobs and in-flight sharing.
use super::*;
use crate::progress::Progress;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Read};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::thread::JoinHandle;
use std::time::Instant;

const WORKERS: usize = 2;
const MAX_JOBS: usize = 32;
const MAX_WAITERS: usize = 128;

enum Event {
    Line(Vec<u8>, mpsc::SyncSender<()>),
    Closed,
    Done(u64, Result<ToolData>),
    Progress(u64, Progress),
}
struct Waiter {
    id: Value,
    shared: bool,
    progress_token: Option<Value>,
}
struct Job {
    key: String,
    params: Value,
    waiters: Vec<Waiter>,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    progress: u64,
    last_progress: Progress,
}
impl Drop for Job {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn notify_progress(
    output: &mut impl Write,
    operation: u64,
    sequence: u64,
    update: &Progress,
    waiters: &[Waiter],
    logs_enabled: bool,
) -> Result<()> {
    for waiter in waiters {
        if let Some(token) = &waiter.progress_token {
            write_response(
                output,
                json!({"jsonrpc":"2.0", "method":"notifications/progress", "params":{
                    "progressToken":token, "progress":sequence, "message":update.message()
                }}),
            )?;
        }
    }
    if logs_enabled && waiters.iter().any(|waiter| waiter.progress_token.is_none()) {
        write_response(
            output,
            json!({"jsonrpc":"2.0", "method":"notifications/message", "params":{
                "level":"info", "logger":"panoptes", "data":{"operationId":operation, "progress":sequence, "stage":update.stage, "completed":update.completed, "total":update.total, "message":update.message()}
            }}),
        )?;
    }
    Ok(())
}

fn timeout_error(id: Value, timeout: Duration, last_progress: &Progress) -> Value {
    error(
        id,
        -32002,
        &format!(
            "No indexing/query progress for {}s; any running worker was stopped and uncommitted index updates rolled back. Last status: {}. Narrow the query or increase --timeout-secs for slow individual stages.",
            timeout.as_secs(),
            last_progress.message()
        ),
    )
}

pub(super) fn serve(
    store: &Path,
    targets: &[repo::Target],
    no_refresh: bool,
    timeout: Duration,
) -> Result<()> {
    // Drop the receiver before cancelling/joining jobs on an I/O error, so a
    // supervisor blocked sending completion cannot deadlock cleanup.
    let mut jobs: HashMap<u64, Job> = HashMap::new();
    let (send, events) = mpsc::sync_channel(64);
    let reader_send = send.clone();
    std::thread::spawn(move || {
        let mut input = std::io::stdin().lock();
        loop {
            let mut line = Vec::new();
            match input
                .by_ref()
                .take((MAX_MESSAGE + 1) as u64)
                .read_until(b'\n', &mut line)
            {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let oversized = line.len() > MAX_MESSAGE;
            // One unread line at a time bounds transport memory even if the
            // client floods requests faster than the scheduler can reject them.
            let (ack, received) = mpsc::sync_channel(0);
            if reader_send.send(Event::Line(line, ack)).is_err() || received.recv().is_err() {
                return;
            }
            if oversized {
                break;
            }
        }
        let _ = reader_send.send(Event::Closed);
    });
    let mut output = std::io::stdout().lock();
    let mut session = SessionStats::default();
    let mut keys: HashMap<String, u64> = HashMap::new();
    let mut queue = VecDeque::new();
    let mut next = 0u64;
    let mut active = 0usize;
    let mut logs_enabled = true;
    loop {
        // Productive workers can run longer than one idle window. Queued calls
        // still need their own inactivity expiry while all worker slots are busy.
        let expired: Vec<_> = jobs
            .iter()
            .filter(|(_, job)| job.thread.is_none() && Instant::now() >= job.deadline)
            .map(|(id, _)| *id)
            .collect();
        for operation in expired {
            let mut job = jobs.remove(&operation).unwrap();
            if keys.get(&job.key) == Some(&operation) {
                keys.remove(&job.key);
            }
            for waiter in job.waiters.drain(..) {
                write_response(
                    &mut output,
                    timeout_error(waiter.id, timeout, &job.last_progress),
                )?;
            }
        }
        queue.retain(|operation| jobs.contains_key(operation));
        while active < WORKERS {
            let Some(operation) = queue.pop_front() else {
                break;
            };
            let Some(job) = jobs.get_mut(&operation) else {
                continue;
            };
            let store = store.to_path_buf();
            let targets = targets.to_vec();
            let params = job.params.clone();
            let cancelled = job.cancelled.clone();
            let deadline = job.deadline;
            let send = send.clone();
            active += 1;
            job.thread = Some(std::thread::spawn(move || {
                let result = worker::run(
                    &store,
                    &targets,
                    params,
                    no_refresh,
                    worker::Limits {
                        deadline,
                        idle_timeout: timeout,
                    },
                    &cancelled,
                    |update| {
                        // Inactivity is tracked by the supervisor even if a slow
                        // client needs display updates coalesced under backpressure.
                        let _ = send.try_send(Event::Progress(operation, update));
                    },
                );
                let _ = send.send(Event::Done(operation, result));
            }));
        }
        let event = if let Some(deadline) = jobs
            .values()
            .filter(|job| job.thread.is_none())
            .map(|job| job.deadline)
            .min()
        {
            match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(event) => event,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(problem) => return Err(problem.into()),
            }
        } else {
            events.recv()?
        };
        match event {
            // EOF is a disconnected client, not a request to drain the queue.
            // Drop the receiver before joining supervisors so blocked sends wake.
            Event::Closed => {
                for job in jobs.values() {
                    job.cancelled.store(true, Ordering::Relaxed);
                }
                drop(events);
                return Ok(());
            }
            Event::Progress(operation, update) => {
                if let Some(job) = jobs.get_mut(&operation) {
                    job.progress += 1;
                    job.last_progress = update;
                    notify_progress(
                        &mut output,
                        operation,
                        job.progress,
                        &job.last_progress,
                        &job.waiters,
                        logs_enabled,
                    )?;
                }
            }
            Event::Done(operation, result) => {
                active -= 1;
                let Some(mut job) = jobs.remove(&operation) else {
                    continue;
                };
                if keys.get(&job.key) == Some(&operation) {
                    keys.remove(&job.key);
                }
                for waiter in job.waiters.drain(..) {
                    let response = match &result {
                        Ok(data) => {
                            let mut data = data.clone();
                            data.value
                                .as_object_mut()
                                .context("tool output must be an object")?
                                .insert(
                                    "panoptesExecution".into(),
                                    json!({"operationId":operation, "coalesced":waiter.shared}),
                                );
                            match tool_response(data, &mut session) {
                                Ok(value) => {
                                    json!({"jsonrpc":"2.0", "id":waiter.id, "result":value})
                                }
                                Err(problem) => error(waiter.id, -32000, &format!("{problem:#}")),
                            }
                        }
                        Err(problem) if problem.is::<worker::TimedOut>() => {
                            timeout_error(waiter.id, timeout, &job.last_progress)
                        }
                        Err(problem) => error(waiter.id, -32000, &format!("{problem:#}")),
                    };
                    write_response(&mut output, response)?;
                }
            }
            Event::Line(line, ack) => {
                let _ = ack.send(());
                if line.len() > MAX_MESSAGE {
                    write_response(&mut output, error(Value::Null, -32600, "request too large"))?;
                    continue;
                }
                let request: Value = match serde_json::from_slice(&line) {
                    Ok(value) => value,
                    Err(_) => {
                        write_response(&mut output, error(Value::Null, -32700, "invalid JSON"))?;
                        continue;
                    }
                };
                let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                let params = request.get("params").cloned().unwrap_or(Value::Null);
                let Some(id) = request.get("id").cloned() else {
                    if method == "notifications/cancelled"
                        && let Some(id) = params.get("requestId")
                    {
                        jobs.retain(|operation, job| {
                            job.waiters.retain(|waiter| &waiter.id != id);
                            if job.waiters.is_empty() {
                                job.cancelled.store(true, Ordering::Relaxed);
                                if keys.get(&job.key) == Some(operation) {
                                    keys.remove(&job.key);
                                }
                                return job.thread.is_some();
                            }
                            true
                        });
                        queue.retain(|operation| jobs.contains_key(operation));
                    }
                    continue;
                };
                if method == "logging/setLevel" {
                    let level = params.get("level").and_then(Value::as_str).unwrap_or("");
                    if !matches!(
                        level,
                        "debug"
                            | "info"
                            | "notice"
                            | "warning"
                            | "error"
                            | "critical"
                            | "alert"
                            | "emergency"
                    ) {
                        write_response(&mut output, error(id, -32602, "invalid log level"))?;
                    } else {
                        logs_enabled = matches!(level, "debug" | "info");
                        write_response(
                            &mut output,
                            json!({"jsonrpc":"2.0", "id":id, "result":{}}),
                        )?;
                    }
                    continue;
                }
                if method == "tools/call" {
                    let progress_token = params
                        .get("_meta")
                        .and_then(|meta| meta.get("progressToken"))
                        .filter(|token| token.is_string() || token.is_number())
                        .cloned();
                    if jobs
                        .values()
                        .flat_map(|job| &job.waiters)
                        .any(|waiter| waiter.id == id)
                    {
                        write_response(
                            &mut output,
                            error(id, -32600, "request id already in flight"),
                        )?;
                        continue;
                    }
                    if jobs.values().map(|job| job.waiters.len()).sum::<usize>() >= MAX_WAITERS {
                        write_response(
                            &mut output,
                            error(
                                id,
                                -32003,
                                "too many pending requests; wait for an existing request to finish",
                            ),
                        )?;
                        continue;
                    }
                    // Transport metadata (for example a per-request progress
                    // token) does not change the tool's result or its identity.
                    let key = serde_json::to_string(&json!({
                        "name": params.get("name"),
                        "arguments": params.get("arguments").filter(|args| !args.is_null()).cloned().unwrap_or_else(|| json!({})),
                    }))?;
                    if let Some(operation) = keys.get(&key) {
                        let job = jobs.get_mut(operation).unwrap();
                        job.waiters.push(Waiter {
                            id,
                            shared: true,
                            progress_token,
                        });
                        notify_progress(
                            &mut output,
                            *operation,
                            job.progress,
                            &job.last_progress,
                            &job.waiters[job.waiters.len() - 1..],
                            logs_enabled,
                        )?;
                    } else if jobs.len() >= MAX_JOBS {
                        write_response(
                            &mut output,
                            error(
                                id,
                                -32003,
                                "query queue is full; wait for an existing request to finish",
                            ),
                        )?;
                    } else {
                        next += 1;
                        keys.insert(key.clone(), next);
                        jobs.insert(
                            next,
                            Job {
                                key,
                                params,
                                waiters: vec![Waiter {
                                    id,
                                    shared: false,
                                    progress_token,
                                }],
                                deadline: Instant::now() + timeout,
                                cancelled: Arc::new(AtomicBool::new(false)),
                                thread: None,
                                progress: 1,
                                last_progress: Progress {
                                    stage: "Queued".into(),
                                    completed: 0,
                                    total: None,
                                    detail: String::new(),
                                },
                            },
                        );
                        let job = jobs.get(&next).unwrap();
                        notify_progress(
                            &mut output,
                            next,
                            job.progress,
                            &job.last_progress,
                            &job.waiters,
                            logs_enabled,
                        )?;
                        queue.push_back(next);
                    }
                } else if matches!(method, "initialize" | "ping" | "tools/list") {
                    let result =
                        dispatch(store, targets, method, &params, no_refresh, &mut session)?;
                    write_response(
                        &mut output,
                        json!({"jsonrpc":"2.0", "id":id, "result":result}),
                    )?;
                } else {
                    write_response(&mut output, error(id, -32601, "method not found"))?;
                }
            }
        }
    }
}
