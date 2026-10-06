//! Exercise deadlines through the real stdio server and isolated workers.
use rusqlite::Connection;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{
    atomic::{AtomicU32, Ordering},
    mpsc::{self, Receiver},
};
use std::time::{Duration, Instant};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "panoptes-timeout-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(root.join("repo")).unwrap();
        let init = Command::new("git")
            .args(["init", "--quiet"])
            .arg(root.join("repo"))
            .output()
            .unwrap();
        assert!(init.status.success());
        let fixture = Self(root.canonicalize().unwrap());
        fixture.source("pub fn original() {}\n");
        fixture
    }

    fn store(&self) -> PathBuf {
        self.0.join("store.db")
    }

    fn source(&self, code: &str) {
        let padding =
            "// This padding makes savings measurable across isolated requests.\n".repeat(200);
        std::fs::write(self.0.join("repo/lib.rs"), format!("{padding}{code}")).unwrap();
    }

    fn build(&self) {
        let output = Command::new(env!("CARGO_BIN_EXE_panoptes"))
            .arg("--store")
            .arg(self.store())
            .arg("build")
            .arg(self.0.join("repo"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Server {
    child: Child,
    input: Option<ChildStdin>,
    replies: Receiver<Value>,
    notifications: Receiver<Value>,
}

impl Server {
    fn start(fixture: &Fixture) -> Self {
        Self::with_timeout(fixture, "1")
    }

    fn with_timeout(fixture: &Fixture, timeout: &str) -> Self {
        Self::with_command(
            fixture,
            timeout,
            Command::new(env!("CARGO_BIN_EXE_panoptes")),
        )
    }

    fn with_command(fixture: &Fixture, timeout: &str, command: Command) -> Self {
        Self::with_path(fixture, timeout, command, fixture.0.join("repo"))
    }

    fn with_path(fixture: &Fixture, timeout: &str, mut command: Command, root: PathBuf) -> Self {
        let mut child = command
            .arg("--store")
            .arg(fixture.store())
            .arg("mcp")
            .arg(root)
            .args(["--timeout-secs", timeout])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (send, replies) = mpsc::channel();
        let (notify, notifications) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str(&line) else {
                    break;
                };
                let value: Value = value;
                let destination = if value.get("id").is_some() {
                    &send
                } else {
                    &notify
                };
                if destination.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input: Some(input),
            replies,
            notifications,
        }
    }

    fn send(&mut self, id: u64, method: &str, params: Value) {
        let input = self.input.as_mut().unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .unwrap();
        input.flush().unwrap();
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(id, method, params);
        let reply = self
            .replies
            .recv_timeout(Duration::from_secs(8))
            .expect("MCP request must respond instead of hanging");
        assert_eq!(reply["id"], id);
        reply
    }

    fn find(&mut self, id: u64, query: &str) -> Value {
        self.request(
            id,
            "tools/call",
            json!({"name":"find", "arguments":{"query":query}}),
        )
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_timeout(reply: &Value, started: Instant) {
    assert_eq!(reply["error"]["code"], -32002, "{reply}");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("worker was stopped")
    );
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "deadline must beat SQLite's five-second busy wait"
    );
}

#[test]
fn demand_indexing_times_out_and_the_same_server_recovers() {
    let fixture = Fixture::new();
    fixture.build();
    let writer = Connection::open(fixture.store()).unwrap();
    writer.execute_batch("begin immediate").unwrap();
    fixture.source("pub fn original() {}\npub fn refreshed() {}\n");
    let mut server = Server::start(&fixture);
    let init = server.request(1, "initialize", json!({}));
    assert_eq!(init["result"]["serverInfo"]["name"], "panoptes");
    let started = Instant::now();
    let reply = server.find(2, "refreshed");
    assert_timeout(&reply, started);
    writer.execute_batch("rollback").unwrap();
    assert_eq!(server.request(3, "ping", json!({}))["result"], json!({}));
    let reply = server.find(4, "refreshed");
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert!(
        reply["result"]["structuredContent"]["repositories"]["repo"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["name"] == "refreshed")
    );
}

#[test]
fn request_refresh_times_out_and_preserves_session_savings() {
    let fixture = Fixture::new();
    let mut server = Server::start(&fixture);
    server.request(1, "initialize", json!({}));
    let first = server.find(2, "original");
    assert_eq!(first["result"]["isError"], false, "{first}");
    let savings = &first["result"]["structuredContent"]["panoptesSavings"];
    let first_total = savings["sessionEstimatedTokensSaved"].as_u64().unwrap();
    assert_eq!(savings["sessionCallsWithSavings"], 1);

    let writer = Connection::open(fixture.store()).unwrap();
    writer.execute_batch("begin immediate").unwrap();
    fixture.source("pub fn original() {}\npub fn refreshed() {}\n");
    let started = Instant::now();
    assert_timeout(&server.find(3, "refreshed"), started);
    writer.execute_batch("rollback").unwrap();

    let reply = server.find(4, "refreshed");
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let savings = &reply["result"]["structuredContent"]["panoptesSavings"];
    assert_eq!(savings["sessionCallsWithSavings"], 2);
    assert!(savings["sessionEstimatedTokensSaved"].as_u64().unwrap() > first_total);
}

#[test]
fn initialize_does_not_start_indexing_and_idle_disconnect_is_prompt() {
    let fixture = Fixture::new();
    fixture.build();
    let writer = Connection::open(fixture.store()).unwrap();
    writer.execute_batch("begin immediate").unwrap();
    fixture.source("pub fn changed() {}\n");
    let mut server = Server::with_timeout(&fixture, "30");
    server.request(1, "initialize", json!({}));
    drop(server.input.take());
    let started = Instant::now();
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "idle disconnect must exit promptly"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    writer.execute_batch("rollback").unwrap();
}

#[test]
fn disconnect_cancels_active_and_queued_requests() {
    let fixture = Fixture::new();
    fixture.build();
    let writer = Connection::open(fixture.store()).unwrap();
    writer.execute_batch("begin immediate").unwrap();
    fixture.source("pub fn changed() {}\n");
    let mut server = Server::with_timeout(&fixture, "30");
    for id in 1..=4 {
        server.send(
            id,
            "tools/call",
            json!({"name":"find", "arguments":{"query":format!("changed {id}")}}),
        );
    }
    server.request(5, "ping", json!({}));
    drop(server.input.take());
    let started = Instant::now();
    loop {
        if let Some(status) = server.child.try_wait().unwrap() {
            assert!(status.success());
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "disconnect must cancel workers and queued calls"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    writer.execute_batch("rollback").unwrap();
    let mut recovered = Server::with_timeout(&fixture, "5");
    assert_eq!(recovered.find(1, "changed")["result"]["isError"], false);
}

#[test]
fn fractional_progress_tokens_are_preserved() {
    let fixture = Fixture::new();
    let mut server = Server::with_timeout(&fixture, "5");
    server.send(
        1,
        "tools/call",
        json!({"name":"find", "arguments":{"query":"original"}, "_meta":{"progressToken":1.5}}),
    );
    assert_eq!(receive(&server)["result"]["isError"], false);
    let updates: Vec<_> = server.notifications.try_iter().collect();
    assert!(!updates.is_empty());
    assert!(
        updates
            .iter()
            .all(|update| update["method"] == "notifications/progress"
                && update["params"]["progressToken"] == 1.5)
    );
}

#[test]
fn worker_has_its_own_deadline_even_without_a_supervising_mcp_parent() {
    let fixture = Fixture::new();
    fixture.build();
    let writer = Connection::open(fixture.store()).unwrap();
    writer.execute_batch("begin immediate").unwrap();
    fixture.source("pub fn changed() {}\n");
    let mut child = Command::new(env!("CARGO_BIN_EXE_panoptes"))
        .arg("__mcp-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    serde_json::to_writer(
        &mut input,
        &json!({
            "store": fixture.store(),
            "targets": [{"label":"repo", "root":fixture.0.join("repo")}],
            "params": {"name":"find", "arguments":{"query":"changed"}},
            "no_refresh": false,
            "timeout_ms": 100,
        }),
    )
    .unwrap();
    drop(input);
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert_eq!(status.code(), Some(124));
            break;
        }
        if started.elapsed() >= Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("worker must stop even without its parent watchdog");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    writer.execute_batch("rollback").unwrap();
}

#[cfg(target_os = "android")]
fn linker_command() -> Option<Command> {
    let linker = if cfg!(target_pointer_width = "64") {
        "/system/bin/linker64"
    } else {
        "/system/bin/linker"
    };
    let mut command = Command::new(linker);
    command
        .arg(env!("CARGO_BIN_EXE_panoptes"))
        .env_remove("LD_PRELOAD")
        // A stale hint from the launcher must not override our actual argv[0].
        .env("TERMUX_EXEC__PROC_SELF_EXE", "/not-the-panoptes-binary");
    // Android 9's linker (also used by termux-docker) prints this exact
    // informational line instead of supporting an explicit executable argument.
    // Skip only that known capability gap; all other failures remain failures.
    let probe = command.args(["version", "--json"]).output().unwrap();
    if probe.status.success()
        && probe.stdout
            == format!("This is {linker}, the helper program for dynamic executables.\n").as_bytes()
    {
        eprintln!("explicit linker launch unsupported by this Android linker");
        return None;
    }
    let mut command = Command::new(linker);
    command
        .arg(env!("CARGO_BIN_EXE_panoptes"))
        .env_remove("LD_PRELOAD")
        .env("TERMUX_EXEC__PROC_SELF_EXE", "/not-the-panoptes-binary");
    Some(command)
}

#[cfg(target_os = "android")]
#[test]
fn android_linker_launch_without_preload_can_index_and_reexecute_workers() {
    let fixture = Fixture::new();
    let Some(command) = linker_command() else {
        return;
    };
    let mut server = Server::with_command(&fixture, "5", command);
    let init = server.request(1, "initialize", json!({}));
    assert_eq!(init["result"]["serverInfo"]["name"], "panoptes");
    let result = server.find(2, "original");
    assert_eq!(result["result"]["isError"], false, "{result}");
    fixture.source("pub fn original() {}\npub fn refreshed() {}\n");
    let result = server.find(3, "refreshed");
    assert_eq!(result["result"]["isError"], false, "{result}");
    assert!(
        result["result"]["structuredContent"]["repositories"]["repo"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["name"] == "refreshed")
    );
}

#[cfg(target_os = "android")]
#[test]
fn android_linker_launch_reports_the_program_path_instead_of_the_loader() {
    let Some(mut command) = linker_command() else {
        return;
    };
    let output = command.args(["version", "--json"]).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let version: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "invalid version JSON: {error}; stdout: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(
        PathBuf::from(version["executable"].as_str().unwrap()),
        std::fs::canonicalize(env!("CARGO_BIN_EXE_panoptes")).unwrap()
    );
}

fn receive(server: &Server) -> Value {
    server
        .replies
        .recv_timeout(Duration::from_secs(8))
        .expect("request must finish")
}

#[test]
fn in_flight_duplicates_share_work_while_ping_remains_responsive() {
    let fixture = Fixture::new();
    fixture.build();
    let writer = Connection::open(fixture.store()).unwrap();
    writer.execute_batch("begin immediate").unwrap();
    fixture.source("pub fn changed() {}\n");
    let mut server = Server::with_timeout(&fixture, "5");
    let params = json!({"name":"find", "arguments":{"query":"changed"}});
    server.send(1, "tools/call", params.clone());
    let mut duplicate = params.clone();
    duplicate["_meta"] = json!({"progressToken":"second-caller"});
    server.send(2, "tools/call", duplicate);
    let ping = server.request(3, "ping", json!({}));
    assert_eq!(ping["result"], json!({}));
    writer.execute_batch("rollback").unwrap();
    let first = receive(&server);
    let second = receive(&server);
    let a = &first["result"]["structuredContent"];
    let b = &second["result"]["structuredContent"];
    assert_eq!(first["id"], 1, "{first}");
    assert_eq!(second["id"], 2, "{second}");
    assert_eq!(a["repositories"]["repo"], b["repositories"]["repo"]);
    assert_eq!(
        a["panoptesExecution"]["operationId"],
        b["panoptesExecution"]["operationId"]
    );
    assert_eq!(a["panoptesExecution"]["coalesced"], false);
    assert_eq!(b["panoptesExecution"]["coalesced"], true);
    fixture.source("pub fn changed() { let updated = 7; }\n");
    let later = server.request(4, "tools/call", params);
    assert_ne!(
        later["result"]["structuredContent"]["panoptesExecution"]["operationId"],
        a["panoptesExecution"]["operationId"]
    );
    assert!(later.to_string().contains("updated"), "{later}");
}

#[test]
fn cancelling_one_duplicate_preserves_the_other_waiter() {
    let fixture = Fixture::new();
    fixture.build();
    let writer = Connection::open(fixture.store()).unwrap();
    writer.execute_batch("begin immediate").unwrap();
    fixture.source("pub fn changed() {}\n");
    let mut server = Server::with_timeout(&fixture, "5");
    let params = json!({"name":"find", "arguments":{"query":"changed"}});
    server.send(1, "tools/call", params.clone());
    server.send(2, "tools/call", params);
    writeln!(
        server.input.as_mut().unwrap(),
        "{}",
        json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":1}})
    )
    .unwrap();
    server.request(3, "ping", json!({}));
    writer.execute_batch("rollback").unwrap();
    let reply = receive(&server);
    assert_eq!(reply["id"], 2, "{reply}");
    assert_eq!(reply["result"]["isError"], false, "{reply}");
}

#[test]
fn concurrent_distinct_queries_reuse_one_checkout_refresh() {
    let fixture = Fixture::new();
    fixture.build();
    let writer = Connection::open(fixture.store()).unwrap();
    writer.execute_batch("create table refreshes(n integer); create trigger refreshed after update of snapshot_id on checkouts when new.snapshot_id is not old.snapshot_id begin insert into refreshes values(1); end; begin immediate;").unwrap();
    fixture.source("pub fn changed() {}\npub fn other() {}\n");
    let mut server = Server::with_timeout(&fixture, "5");
    server.send(
        1,
        "tools/call",
        json!({"name":"find", "arguments":{"query":"changed"}}),
    );
    server.send(
        2,
        "tools/call",
        json!({"name":"find", "arguments":{"query":"other"}}),
    );
    server.request(3, "ping", json!({}));
    writer.execute_batch("rollback").unwrap();
    for _ in 0..2 {
        let reply = receive(&server);
        assert_eq!(reply["result"]["isError"], false, "{reply}");
    }
    let builds: i64 = writer
        .query_row("select count(*) from refreshes", [], |row| row.get(0))
        .unwrap();
    assert_eq!(builds, 1, "waiting workers must reuse the first refresh");
}

#[test]
fn discovers_new_worktree_and_keeps_concurrent_checkout_queries_separate() {
    let fixture = Fixture::new();
    let mut server = Server::with_timeout(&fixture, "5");
    server.request(1, "initialize", json!({}));
    assert!(
        !fixture.store().exists(),
        "initialization must not build a store"
    );
    let worktree = fixture.0.join("review");
    let output = Command::new("git")
        .arg("-C")
        .arg(fixture.0.join("repo"))
        .args(["worktree", "add", "--orphan", "-b", "pr-review"])
        .arg(&worktree)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    std::fs::write(worktree.join("lib.rs"), "pub fn review_only() {}\n").unwrap();
    let discovered = server.request(2, "tools/call", json!({"name":"worktrees", "arguments":{}}));
    let trees = discovered["result"]["structuredContent"]["worktrees"]
        .as_array()
        .unwrap();
    assert_eq!(trees.len(), 2);
    assert!(trees.iter().any(|tree| tree["root"] == worktree.to_str().unwrap() && tree["branch"] == "pr-review"));
    assert!(
        !fixture.store().exists(),
        "discovery must not open an index"
    );
    server.send(
        3,
        "tools/call",
        json!({"name":"grep", "arguments":{"pattern":"pub fn", "repo":worktree}}),
    );
    server.send(
        4,
        "tools/call",
        json!({"name":"grep", "arguments":{"pattern":"pub fn"}}),
    );
    let mut replies = [receive(&server), receive(&server)];
    replies.sort_by_key(|reply| reply["id"].as_u64());
    assert_eq!(replies[0]["result"]["isError"], false, "{}", replies[0]);
    assert_eq!(replies[1]["result"]["isError"], false, "{}", replies[1]);
    assert!(replies[0].to_string().contains("review_only"));
    assert!(!replies[0].to_string().contains("original()"));
    assert!(replies[1].to_string().contains("original()"));
    assert!(!replies[1].to_string().contains("review_only"));
    assert_ne!(
        replies[0]["result"]["structuredContent"]["panoptesExecution"]["operationId"],
        replies[1]["result"]["structuredContent"]["panoptesExecution"]["operationId"]
    );
    let by_label = server.request(
        5,
        "tools/call",
        json!({"name":"find", "arguments":{"query":"review_only", "repo":"review"}}),
    );
    assert_eq!(
        by_label["result"]["structuredContent"]["panoptesCheckouts"][0]["branch"],
        "pr-review"
    );
}

#[test]
fn bounded_queue_rejects_overload_and_recovers_after_cancellation() {
    let fixture = Fixture::new();
    fixture.build();
    let writer = Connection::open(fixture.store()).unwrap();
    writer.execute_batch("begin immediate").unwrap();
    fixture.source("pub fn changed() {}\n");
    let mut server = Server::with_timeout(&fixture, "10");
    for id in 1..=33 {
        server.send(
            id,
            "tools/call",
            json!({"name":"find", "arguments":{"query":format!("changed {id}")}}),
        );
    }
    let reply = receive(&server);
    assert_eq!(reply["id"], 33, "{reply}");
    assert_eq!(reply["error"]["code"], -32003, "{reply}");
    for id in 1..=32 {
        writeln!(
            server.input.as_mut().unwrap(),
            "{}",
            json!({"jsonrpc":"2.0", "method":"notifications/cancelled", "params":{"requestId":id}})
        )
        .unwrap();
    }
    server.request(34, "ping", json!({}));
    writer.execute_batch("rollback").unwrap();
    // The cancelled jobs must release queue capacity as well as their workers.
    let reply = server.find(35, "changed");
    assert_eq!(reply["result"]["isError"], false, "{reply}");
}

#[test]
fn review_metadata_names_preserve_checkout_hits() {
    let fixture = Fixture::new();
    let mut server = Server::with_timeout(&fixture, "5");
    server.request(1, "initialize", json!({}));
    for (offset, label) in [
        "panoptesCheckouts",
        "panoptesExecution",
        "panoptesSavings",
        "repositories",
        "worktrees",
    ]
    .iter()
    .enumerate()
    {
        let root = fixture.0.join(label);
        let output = Command::new("git")
            .args(["init", "--quiet"])
            .arg(&root)
            .output()
            .unwrap();
        assert!(output.status.success());
        std::fs::copy(fixture.0.join("repo/lib.rs"), root.join("lib.rs")).unwrap();
        let response = server.request(
            offset as u64 + 2,
            "tools/call",
            json!({"name":"find", "arguments":{"query":"original", "repo":root}}),
        );
        assert_eq!(response["result"]["isError"], false, "{response}");
        let data = &response["result"]["structuredContent"];
        assert_eq!(
            data["repositories"][*label]["hits"][0]["name"], "original",
            "{response}"
        );
        assert_eq!(data["panoptesCheckouts"][0]["label"], *label);
        assert!(data["panoptesExecution"]["operationId"].is_number());
        assert!(data["panoptesSavings"]["sessionSavingsDisplay"].is_string());
    }
}

fn add_orphan_worktree(fixture: &Fixture, name: &str) -> PathBuf {
    let root = fixture.0.join(name);
    let output = Command::new("git")
        .arg("-C")
        .arg(fixture.0.join("repo"))
        .args(["worktree", "add", "--orphan", "-b", name])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    root
}

#[test]
fn review_removed_startup_worktree_discovers_survivors() {
    let fixture = Fixture::new();
    let review = add_orphan_worktree(&fixture, "review-start");
    let mut server = Server::with_path(
        &fixture,
        "5",
        Command::new(env!("CARGO_BIN_EXE_panoptes")),
        review.clone(),
    );
    server.request(1, "initialize", json!({}));
    let removed = Command::new("git")
        .arg("-C")
        .arg(fixture.0.join("repo"))
        .args(["worktree", "remove"])
        .arg(&review)
        .output()
        .unwrap();
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!review.exists());
    let sibling = add_orphan_worktree(&fixture, "review-sibling");
    let response = server.request(2, "tools/call", json!({"name":"worktrees", "arguments":{}}));
    let trees = response["result"]["structuredContent"]["worktrees"]
        .as_array()
        .unwrap();
    assert_eq!(trees.len(), 2, "{response}");
    assert!(
        trees
            .iter()
            .all(|tree| PathBuf::from(tree["root"].as_str().unwrap()).is_dir())
    );
    assert!(
        trees
            .iter()
            .any(|tree| tree["root"] == sibling.to_str().unwrap())
    );
    assert!(
        trees
            .iter()
            .any(|tree| tree["root"] == fixture.0.join("repo").to_str().unwrap())
    );
    assert!(
        !fixture.store().exists(),
        "discovery must remain observational"
    );
    let missing = server.find(3, "original");
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no longer exists"),
        "{missing}"
    );
    assert!(
        !fixture.store().exists(),
        "missing checkout must not create an index"
    );
    let result = server.request(
        4,
        "tools/call",
        json!({"name":"find", "arguments":{"query":"original", "repo":"repo"}}),
    );
    assert_eq!(
        result["result"]["structuredContent"]["repositories"]["repo"]["hits"][0]["name"],
        "original",
        "{result}"
    );
}

#[test]
fn progress_notifications_stream_stages_and_keep_request_tokens() {
    let fixture = Fixture::new();
    let mut server = Server::with_timeout(&fixture, "5");
    server.request(1, "initialize", json!({}));
    server.send(2, "tools/call", json!({"name":"find", "arguments":{"query":"original"}, "_meta":{"progressToken":"index-progress"}}));
    let first = server
        .notifications
        .recv_timeout(Duration::from_secs(2))
        .unwrap();
    assert_eq!(first["method"], "notifications/progress");
    assert_eq!(first["params"]["progressToken"], "index-progress");
    assert_eq!(first["params"]["message"], "Queued");
    let response = receive(&server);
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["isError"], false, "{response}");
    let updates: Vec<_> = std::iter::once(first)
        .chain(server.notifications.try_iter())
        .collect();
    assert!(
        updates
            .iter()
            .all(|update| update["params"]["progressToken"] == "index-progress")
    );
    assert!(
        updates
            .windows(2)
            .all(|pair| pair[0]["params"]["progress"].as_u64().unwrap()
                < pair[1]["params"]["progress"].as_u64().unwrap())
    );
    assert!(updates.iter().any(|update| {
        update["params"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Parsing files")
    }));
    assert!(updates.iter().any(|update| {
        update["params"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Index committed")
    }));
    assert!(updates.iter().any(
        |update| update["params"]["_meta"]["panoptesTiming"]["stage"] == "Waiting for index writer"
    ));
    assert_eq!(
        updates
            .iter()
            .filter(
                |update| update["params"]["_meta"]["panoptesTiming"]["stage"] == "Querying index"
            )
            .count(),
        1
    );
    server.request(3, "ping", json!({}));
    assert!(
        server.notifications.try_recv().is_err(),
        "progress stops when the operation completes"
    );
}

#[test]
fn progress_logs_work_without_tokens_and_respect_log_level() {
    let fixture = Fixture::new();
    let mut server = Server::with_timeout(&fixture, "5");
    server.find(1, "original");
    let updates: Vec<_> = server.notifications.try_iter().collect();
    assert!(
        updates
            .iter()
            .any(|update| update["method"] == "notifications/message"
                && update["params"]["data"]["stage"] == "Index committed")
    );
    server.request(2, "logging/setLevel", json!({"level":"warning"}));
    server.find(3, "original");
    assert!(server.notifications.try_recv().is_err());
    let invalid = server.request(4, "logging/setLevel", json!({"level":"verbose"}));
    assert_eq!(invalid["error"]["code"], -32602);
}

#[test]
fn no_refresh_uses_captured_source_when_the_live_file_is_unreadable_as_text() {
    let fixture = Fixture::new();
    fixture.build();
    std::fs::write(fixture.0.join("repo/lib.rs"), [255u8]).unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_panoptes"));
    command.arg("--no-refresh");
    let mut server = Server::with_command(&fixture, "5", command);
    let result = server.find(1, "original");
    assert_eq!(result["result"]["isError"], false, "{result}");
    let data = &result["result"]["structuredContent"];
    assert_eq!(data["panoptesCheckouts"][0]["sourceComplete"], true);
    assert!(
        data["repositories"]["repo"]["hits"]
            .as_array()
            .unwrap()
            .iter()
            .any(|hit| hit["source"]
                .as_str()
                .is_some_and(|source| source.contains("pub fn original")))
    );
}

#[test]
fn failed_query_flushes_its_final_worker_phase() {
    let fixture = Fixture::new();
    fixture.build();
    let mut server = Server::with_timeout(&fixture, "5");
    let response = server.request(
        1,
        "tools/call",
        json!({"name":"grep","arguments":{"pattern":"["},"_meta":{"progressToken":"failed-query"}}),
    );
    assert_eq!(response["error"]["code"], -32000, "{response}");
    let timings: Vec<_> = server
        .notifications
        .try_iter()
        .filter(|update| update["params"]["_meta"]["panoptesTiming"]["stage"] == "Querying index")
        .collect();
    assert_eq!(timings.len(), 1);
    assert_eq!(timings[0]["params"]["progressToken"], "failed-query");
}

#[test]
fn shared_extraction_progress_counts_misses_and_mixed_hits() {
    for seeded in [false, true] {
        let fixture = Fixture::new();
        if seeded {
            fixture.build();
        }
        let second = fixture.0.join("second");
        std::fs::create_dir_all(second.join(".git")).unwrap();
        std::fs::write(second.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::copy(fixture.0.join("repo/lib.rs"), second.join("lib.rs")).unwrap();
        std::fs::write(second.join("fresh.rs"), "pub fn newly_added() {}\n").unwrap();
        let mut server = Server::with_timeout(&fixture, "5");
        let response = server.request(1,"tools/call",json!({"name":"find","arguments":{"query":"newly_added","repo":second},"_meta":{"progressToken":"lookup"}}));
        assert_eq!(response["result"]["isError"], false, "{response}");
        let messages: Vec<_> = server
            .notifications
            .try_iter()
            .filter_map(|v| v["params"]["message"].as_str().map(str::to_string))
            .filter(|m| m.starts_with("Looking up shared extractions "))
            .collect();
        assert!(
            messages
                .iter()
                .any(|m| m.starts_with("Looking up shared extractions 2/2")),
            "seeded={seeded}: {messages:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn skeleton_cli_preserves_literal_backslashes_in_relative_and_absolute_paths() {
    let fixture = Fixture::new();
    let root = fixture.0.join("repo");
    std::fs::create_dir_all(root.join("a")).unwrap();
    std::fs::write(root.join("a\\b.rs"), "pub fn literal_file() {}\n").unwrap();
    std::fs::write(root.join("a/b.rs"), "pub fn nested_file() {}\n").unwrap();
    fixture.build();
    for file in [PathBuf::from("a\\b.rs"), root.join("a\\b.rs")] {
        let output = Command::new(env!("CARGO_BIN_EXE_panoptes"))
            .current_dir(&root)
            .arg("--store")
            .arg(fixture.store())
            .args(["--no-refresh", "skeleton"])
            .arg(file)
            .arg("--path")
            .arg(&root)
            .arg("--json")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("literal_file"), "{text}");
        assert!(!text.contains("nested_file"), "{text}");
    }
}

#[test]
fn scoped_cli_full_implies_source_for_repository_and_lineage() {
    let fixture = Fixture::new();
    let root = fixture.0.join("repo");
    let commit = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args([
            "-c",
            "user.name=Panoptes Test",
            "-c",
            "user.email=panoptes-test@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "Scoped CLI fixture ancestry",
        ])
        .output()
        .unwrap();
    assert!(
        commit.status.success(),
        "{}",
        String::from_utf8_lossy(&commit.stderr)
    );
    fixture.build();
    let identity = Command::new(env!("CARGO_BIN_EXE_panoptes"))
        .arg("--store")
        .arg(fixture.store())
        .arg("identity")
        .arg(&root)
        .arg("--lineage")
        .output()
        .unwrap();
    assert!(
        identity.status.success(),
        "{}",
        String::from_utf8_lossy(&identity.stderr)
    );
    let identity: Value = serde_json::from_slice(&identity.stdout).unwrap();
    let lineage = format!(
        "{}:{}",
        identity["ancestry"]["objectFormat"].as_str().unwrap(),
        identity["ancestry"]["roots"][0].as_str().unwrap()
    );
    for scope in ["repository", "lineage"] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_panoptes"));
        command
            .arg("--store")
            .arg(fixture.store())
            .args(["--no-refresh", "ask", "original"])
            .arg(&root)
            .args(["--full", "--view-scope", scope]);
        if scope == "lineage" {
            command.args(["--lineage-root", &lineage]);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert!(
            value["repositories"]["repo"]["hits"]
                .as_array()
                .unwrap()
                .iter()
                .any(|hit| hit["source"]
                    .as_str()
                    .is_some_and(|s| s.contains("pub fn original"))),
            "{value}"
        );
    }
}
