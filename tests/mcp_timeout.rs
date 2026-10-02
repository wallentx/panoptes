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
        let fixture = Self(root);
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
}

impl Server {
    fn start(fixture: &Fixture) -> Self {
        Self::with_timeout(fixture, "1")
    }

    fn with_timeout(fixture: &Fixture, timeout: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_panoptes"))
            .arg("--store")
            .arg(fixture.store())
            .arg("mcp")
            .arg(fixture.0.join("repo"))
            .args(["--timeout-secs", timeout])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (send, replies) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                let Ok(line) = line else { break };
                let Ok(value) = serde_json::from_str(&line) else {
                    break;
                };
                if send.send(value).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            input: Some(input),
            replies,
        }
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        let input = self.input.as_mut().unwrap();
        writeln!(
            input,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .unwrap();
        input.flush().unwrap();
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
fn startup_indexing_times_out_and_the_same_server_recovers() {
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
        reply["result"]["structuredContent"]["repo"]["hits"]
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
fn disconnect_cancels_background_indexing_without_waiting_for_its_deadline() {
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
            "disconnect must cancel background indexing promptly"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    writer.execute_batch("rollback").unwrap();
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
