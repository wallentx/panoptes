//! Bounded newline-delimited JSON-RPC server with automatic repository indexing.

use anyhow::{Context, Result, anyhow};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::path::Path;

use crate::{ask, db, index, repo};

const MAX_MESSAGE: usize = 1024 * 1024;
const MAX_OUTPUT: usize = 1024 * 1024;
const TOKEN_CHARS: u64 = 4;
const SAVINGS_LABEL: &str = "ꙮ Estimated tokens saved for this session";

#[derive(Default)]
struct SessionStats {
    estimated_tokens_saved: u64,
    calls_with_savings: u64,
}

struct ToolData {
    value: Value,
    baseline_bytes: u64,
    baseline_files: usize,
}

pub fn serve(store: &Path, start: &Path, no_refresh: bool) -> Result<()> {
    let targets = repo::automatic_targets(start)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut session = SessionStats::default();
    let mut indexing = None;
    for line in stdin.lock().lines() {
        let line = line?;
        if line.len() > MAX_MESSAGE {
            write_response(&mut stdout, error(Value::Null, -32600, "request too large"))?;
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(_) => {
                write_response(&mut stdout, error(Value::Null, -32700, "parse error"))?;
                continue;
            }
        };
        let id = request.get("id").cloned();
        if id.is_none() {
            continue;
        }
        let id = id.unwrap_or(Value::Null);
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        if method == "tools/call" {
            finish_indexing(&mut indexing);
        }
        let response = if !matches!(method, "initialize" | "ping" | "tools/list" | "tools/call") {
            error(id, -32601, &format!("method not found: {method}"))
        } else {
            match dispatch(store, &targets, method, &params, no_refresh, &mut session) {
                Ok(result) => json!({"jsonrpc":"2.0", "id":id, "result":result}),
                Err(problem) => error(id, -32000, &problem.to_string()),
            }
        };
        write_response(&mut stdout, response)?;
        if method == "initialize" && indexing.is_none() {
            indexing = start_indexing(store, &targets, no_refresh);
        }
    }
    Ok(())
}

fn write_response(output: &mut impl Write, response: Value) -> Result<()> {
    let mut bytes = serde_json::to_vec(&response)?;
    if bytes.len() > MAX_OUTPUT {
        bytes = serde_json::to_vec(&error(
            response.get("id").cloned().unwrap_or(Value::Null),
            -32001,
            "response too large; narrow the query",
        ))?;
    }
    output.write_all(&bytes)?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "error":{"code":code, "message":message}})
}

fn dispatch(
    store: &Path,
    targets: &[repo::Target],
    method: &str,
    params: &Value,
    no_refresh: bool,
    session: &mut SessionStats,
) -> Result<Value> {
    match method {
        "initialize" => {
            let instructions = if targets.is_empty() {
                "Panoptes started without indexing because the current directory is not a Git repository or a workspace containing at least two immediate Git repositories. Start the coding agent inside a repository to enable Panoptes source navigation."
            } else {
                "Prefer Panoptes before built-in grep, search, or whole-file reads for indexed source; one focused call usually replaces several reads. Use find for where/how questions (bounded source is included by default), grep when every occurrence matters, callers for incoming/outgoing dependency and blast-radius tracing, skeleton for a file API, and map for orientation. For a scoped question, start with one find or grep rather than map, and treat three Panoptes calls as the normal task budget. Work from returned paths and spans; do not repeat find to reconstruct individual files, and fall back only when indexed context is insufficient. Retrieval results include an honest four-characters-per-token estimate versus reading matched files whole plus an MCP-session total. When Panoptes was used, end the response with the provided session savings display in the form `ꙮ Estimated tokens saved for this session: 1,231,578`; treat it as an estimate, never as billing. Panoptes indexes on connection and refreshes changed source before answers; freshness is observational."
            };
            Ok(json!({
                "protocolVersion":"2024-11-05",
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"panoptes", "version":env!("CARGO_PKG_VERSION")},
                "instructions":instructions
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_schemas()})),
        "tools/call" => {
            let name = params
                .get("name")
                .and_then(Value::as_str)
                .context("missing tool name")?;
            let args = params.get("arguments").unwrap_or(&Value::Null);
            let output = call_tool_detailed(store, targets, name, args, no_refresh)?;
            let data = add_savings(output, session)?;
            Ok(json!({
                "content":[{"type":"text", "text":serde_json::to_string_pretty(&data)?}],
                "structuredContent":data,
                "isError":false
            }))
        }
        _ => unreachable!("method validated by serve"),
    }
}

fn start_indexing(
    store: &Path,
    targets: &[repo::Target],
    no_refresh: bool,
) -> Option<std::thread::JoinHandle<Result<()>>> {
    if no_refresh || targets.is_empty() {
        return None;
    }
    let store = store.to_path_buf();
    let targets = targets.to_vec();
    Some(std::thread::spawn(move || {
        ensure_targets(&store, &targets, false)
    }))
}

fn finish_indexing(worker: &mut Option<std::thread::JoinHandle<Result<()>>>) {
    let Some(worker) = worker.take() else {
        return;
    };
    match worker.join() {
        Ok(Ok(())) => {}
        Ok(Err(problem)) => eprintln!("[panoptes] background indexing deferred: {problem:#}"),
        Err(_) => eprintln!("[panoptes] background indexing worker panicked"),
    }
}

fn ensure_targets(store: &Path, targets: &[repo::Target], no_refresh: bool) -> Result<()> {
    if no_refresh {
        return Ok(());
    }
    for target in targets {
        let mut conn = db::open(store)?;
        if !index::freshness(&conn, &target.root)?.is_clean() {
            index::build(&mut conn, &target.root)?;
        }
    }
    Ok(())
}

fn tool_schemas() -> Vec<Value> {
    vec![
        schema(
            "find",
            "Start here for where/how questions. Returns ranked definitions with exact spans, signatures, graph context, and a bounded source excerpt by default; prefer this over broad search and whole-file reads.",
            &["query"],
        ),
        schema(
            "grep",
            "Use when every textual occurrence matters. Exhaustively searches indexed source and groups regex or literal matches under their enclosing definitions.",
            &["pattern"],
        ),
        schema(
            "callers",
            "Trace incoming callers or outgoing dependencies for a symbol. Use this for dependency chains, impact analysis, and blast radius instead of searching names manually.",
            &["symbol"],
        ),
        schema(
            "skeleton",
            "Inspect a file's definitions, signatures, and line spans without reading the whole implementation.",
            &["file"],
        ),
        schema(
            "map",
            "Orient in an unfamiliar repository using directory clusters, dependency hubs, and hotspots before making a narrower query.",
            &[],
        ),
        schema(
            "status",
            "Show indexed graph counts together with current source freshness.",
            &[],
        ),
        schema(
            "freshness",
            "Observe differences between indexed and live source without triggering a rebuild.",
            &[],
        ),
    ]
}

fn schema(name: &str, description: &str, required: &[&str]) -> Value {
    let properties = match name {
        "find" => {
            json!({
                "query":{"type":"string", "description":"Natural-language or structural code question."},
                "limit":{"type":"integer", "minimum":1, "description":"Maximum ranked definitions to return; defaults to 8."},
                "in":{"type":"string", "description":"Optional repository-relative path scope."},
                "source":{"type":"boolean", "default":true, "description":"Include bounded source excerpts; defaults to true."},
                "full":{"type":"boolean", "default":false, "description":"Return each complete matched definition instead of the bounded excerpt."},
                "repo":{"type":"string", "description":"Workspace repository label; omit to search every repository."}
            })
        }
        "grep" => {
            json!({
                "pattern":{"type":"string", "description":"Regular expression, or literal text when fixed is true."},
                "fixed":{"type":"boolean", "default":false, "description":"Treat pattern as literal text."},
                "ignoreCase":{"type":"boolean", "default":false, "description":"Match without case sensitivity."},
                "in":{"type":"string", "description":"Optional repository-relative path scope."},
                "repo":{"type":"string", "description":"Workspace repository label; omit to search every repository."}
            })
        }
        "callers" => {
            json!({
                "symbol":{"type":"string", "description":"Exact or uniquely resolvable symbol name."},
                "direction":{"enum":["in","out"], "default":"in", "description":"in finds callers; out finds dependencies called by the symbol."},
                "depth":{"type":"integer", "minimum":1, "maximum":32, "default":1, "description":"Maximum graph traversal depth."},
                "in":{"type":"string", "description":"Optional repository-relative path scope."},
                "repo":{"type":"string", "description":"Workspace repository label; omit to search every repository."}
            })
        }
        "skeleton" => json!({
            "file":{"type":"string", "description":"Repository-relative file path or unique filename."},
            "repo":{"type":"string", "description":"Workspace repository label; omit to query every repository."}
        }),
        _ => {
            json!({"repo":{"type":"string", "description":"Workspace repository label; omit to query every repository."}})
        }
    };
    json!({
        "name":name,
        "description":description,
        "inputSchema":{"type":"object", "properties":properties, "required":required, "additionalProperties":false},
        // Retrieval never changes the source workspace or reaches an external
        // system. Panoptes may refresh its own local index cache first.
        "annotations":{
            "readOnlyHint":true,
            "destructiveHint":false,
            "idempotentHint":true,
            "openWorldHint":false
        }
    })
}

#[cfg(test)]
fn call_tool(
    store: &Path,
    targets: &[repo::Target],
    name: &str,
    args: &Value,
    no_refresh: bool,
) -> Result<Value> {
    Ok(call_tool_detailed(store, targets, name, args, no_refresh)?.value)
}

fn call_tool_detailed(
    store: &Path,
    targets: &[repo::Target],
    name: &str,
    args: &Value,
    no_refresh: bool,
) -> Result<ToolData> {
    match name {
        "find" => {
            string_arg(args, "query")?;
        }
        "grep" => {
            string_arg(args, "pattern")?;
        }
        "callers" => {
            string_arg(args, "symbol")?;
        }
        "skeleton" => {
            string_arg(args, "file")?;
        }
        "map" | "status" | "freshness" => {}
        _ => return Err(anyhow!("unknown tool: {name}")),
    }
    let selected = select_targets(targets, args.get("repo").and_then(Value::as_str))?;
    let mut results = serde_json::Map::new();
    let mut baseline_bytes = 0u64;
    let mut baseline_files = 0usize;
    for target in selected {
        let mut conn = db::open(store)?;
        let mut state = index::freshness(&conn, &target.root)?;
        if name == "freshness" {
            results.insert(target.label.clone(), serde_json::to_value(state)?);
            continue;
        }
        if !state.indexed {
            if no_refresh {
                return Err(anyhow!(
                    "{} has not been indexed by Panoptes and automatic indexing is disabled",
                    target.root.display()
                ));
            }
            index::build(&mut conn, &target.root)?;
            if name == "status" {
                state = index::freshness(&conn, &target.root)?;
            }
        } else if !state.is_clean() && !no_refresh {
            index::build(&mut conn, &target.root)?;
            if name == "status" {
                state = index::freshness(&conn, &target.root)?;
            }
        }
        let repo_id = index::repo_id_of(&conn, &target.root)?.context("current index missing")?;
        let value = match name {
            "find" => serde_json::to_value(ask::ask(
                &conn,
                repo_id,
                &target.root,
                string_arg(args, "query")?,
                ask::AskOptions {
                    limit: usize_arg(args, "limit", 8),
                    scope: args.get("in").and_then(Value::as_str),
                    source: bool_arg_default(args, "source", true),
                    full: bool_arg(args, "full"),
                },
            )?)?,
            "grep" => serde_json::to_value(index::grep_with_options(
                &conn,
                repo_id,
                &target.root,
                string_arg(args, "pattern")?,
                index::GrepOptions {
                    ignore_case: bool_arg_named(args, "ignoreCase"),
                    fixed: bool_arg(args, "fixed"),
                    scope: args.get("in").and_then(Value::as_str),
                },
            )?)?,
            "callers" => {
                let (seeds, reached) = index::callers_scoped(
                    &conn,
                    repo_id,
                    string_arg(args, "symbol")?,
                    args.get("direction").and_then(Value::as_str) == Some("out"),
                    usize_arg(args, "depth", 1).min(32),
                    args.get("in").and_then(Value::as_str),
                )?;
                json!({"seeds":seeds, "reached":reached})
            }
            "skeleton" => {
                let requested = string_arg(args, "file")?;
                let rel = index::skeleton_path(&conn, repo_id, requested)?
                    .with_context(|| format!("no unique indexed file matching {requested:?}"))?;
                json!({"path":rel, "symbols":index::skeleton(&conn, repo_id, &rel)?})
            }
            "map" => serde_json::to_value(index::repo_map(&conn, repo_id, 12)?)?,
            "status" => json!({
                "root":target.root,
                "store":index::repo_status(&conn, repo_id)?,
                "freshness":state,
            }),
            _ => unreachable!("tool name validated before indexing"),
        };
        if matches!(name, "find" | "grep" | "callers" | "skeleton" | "map") {
            let mut paths = HashSet::new();
            collect_result_paths(&value, &mut paths);
            for path in paths {
                let size = conn
                    .query_row(
                        "select size from files where repo_id=?1 and path=?2",
                        rusqlite::params![repo_id, path],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;
                if let Some(size) = size {
                    baseline_bytes = baseline_bytes.saturating_add(size.max(0) as u64);
                    baseline_files += 1;
                }
            }
        }
        results.insert(target.label.clone(), value);
    }
    Ok(ToolData {
        value: Value::Object(results),
        baseline_bytes,
        baseline_files,
    })
}

fn collect_result_paths(value: &Value, paths: &mut HashSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(path) = object.get("path").and_then(Value::as_str) {
                paths.insert(path.to_string());
            }
            for child in object.values() {
                collect_result_paths(child, paths);
            }
        }
        Value::Array(values) => {
            for child in values {
                collect_result_paths(child, paths);
            }
        }
        _ => {}
    }
}

fn estimated_tokens(chars_or_bytes: u64) -> u64 {
    chars_or_bytes.div_ceil(TOKEN_CHARS)
}

fn savings_percent(saved: u64, baseline: u64) -> u64 {
    saved.saturating_mul(100).checked_div(baseline).unwrap_or(0)
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut formatted = String::with_capacity(digits.len() + (digits.len() - 1) / 3);
    for (index, digit) in digits.bytes().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            formatted.push(',');
        }
        formatted.push(char::from(digit));
    }
    formatted
}

fn session_savings_display(value: u64) -> String {
    format!("{SAVINGS_LABEL}: {}", format_count(value))
}

fn add_savings(mut output: ToolData, session: &mut SessionStats) -> Result<Value> {
    if output.baseline_bytes == 0 {
        return Ok(output.value);
    }
    let baseline_tokens = estimated_tokens(output.baseline_bytes);
    let mut payload_tokens =
        estimated_tokens(serde_json::to_string_pretty(&output.value)?.chars().count() as u64);
    let mut saved = baseline_tokens.saturating_sub(payload_tokens);

    for _ in 0..4 {
        let session_total = session.estimated_tokens_saved.saturating_add(saved);
        let session_calls = session.calls_with_savings + u64::from(saved > 0);
        let metadata = json!({
            "estimatedTokensSaved":saved,
            "estimatedSavingsPercent":savings_percent(saved, baseline_tokens),
            "responseTokens":payload_tokens,
            "baselineTokens":baseline_tokens,
            "matchedFiles":output.baseline_files,
            "sessionEstimatedTokensSaved":session_total,
            "sessionCallsWithSavings":session_calls,
            "sessionSavingsDisplay":session_savings_display(session_total),
            "basis":"estimate at four UTF-8 source bytes or output characters per token versus reading matched source files whole; not model billing"
        });
        output
            .value
            .as_object_mut()
            .context("tool output must be an object")?
            .insert("panoptesSavings".to_string(), metadata);
        let next_payload_tokens =
            estimated_tokens(serde_json::to_string_pretty(&output.value)?.chars().count() as u64);
        let next_saved = baseline_tokens.saturating_sub(next_payload_tokens);
        if next_payload_tokens == payload_tokens && next_saved == saved {
            break;
        }
        payload_tokens = next_payload_tokens;
        saved = next_saved;
    }

    if saved > 0 {
        session.estimated_tokens_saved = session.estimated_tokens_saved.saturating_add(saved);
        session.calls_with_savings += 1;
    }
    let metadata = json!({
        "estimatedTokensSaved":saved,
        "estimatedSavingsPercent":savings_percent(saved, baseline_tokens),
        "responseTokens":payload_tokens,
        "baselineTokens":baseline_tokens,
        "matchedFiles":output.baseline_files,
        "sessionEstimatedTokensSaved":session.estimated_tokens_saved,
        "sessionCallsWithSavings":session.calls_with_savings,
        "sessionSavingsDisplay":session_savings_display(session.estimated_tokens_saved),
        "basis":"estimate at four UTF-8 source bytes or output characters per token versus reading matched source files whole; not model billing"
    });
    output
        .value
        .as_object_mut()
        .context("tool output must be an object")?
        .insert("panoptesSavings".to_string(), metadata);
    Ok(output.value)
}

fn select_targets<'a>(
    targets: &'a [repo::Target],
    label: Option<&str>,
) -> Result<Vec<&'a repo::Target>> {
    if targets.is_empty() {
        return Err(anyhow!(
            "current directory is not a Git repository or a workspace containing at least two immediate Git repositories"
        ));
    }
    match label {
        None => Ok(targets.iter().collect()),
        Some(label) => targets
            .iter()
            .find(|target| target.label == label)
            .map(|target| vec![target])
            .with_context(|| format!("unknown workspace repo {label:?}")),
    }
}

fn string_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {name:?}"))
}

fn usize_arg(args: &Value, name: &str, default: usize) -> usize {
    args.get(name)
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(default)
}

fn bool_arg(args: &Value, name: &str) -> bool {
    bool_arg_named(args, name)
}

fn bool_arg_default(args: &Value, name: &str, default: bool) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(default)
}

fn bool_arg_named(args: &Value, name: &str) -> bool {
    args.get(name).and_then(Value::as_bool).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "panoptes-mcp-{name}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(name: &str) -> (TempDir, std::path::PathBuf, repo::Target) {
        let temp = TempDir::new(name);
        let root = temp.0.join("repo");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("lib.rs"), "pub fn original() {}\n").unwrap();
        let store = temp.0.join("panoptes.db");
        let target = repo::Target {
            label: "repo".to_string(),
            root,
        };
        (temp, store, target)
    }

    #[test]
    fn tool_list_has_unique_names_and_object_schemas() {
        let tools = tool_schemas();
        let names: std::collections::HashSet<_> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), tools.len());
        assert!(
            tools
                .iter()
                .all(|tool| tool["inputSchema"]["type"] == "object")
        );
        assert!(tools.iter().all(|tool| {
            tool["annotations"]["readOnlyHint"] == true
                && tool["annotations"]["destructiveHint"] == false
                && tool["annotations"]["idempotentHint"] == true
                && tool["annotations"]["openWorldHint"] == false
        }));
    }

    #[test]
    fn freshness_is_observational_but_other_tools_build_and_refresh() {
        let (_temp, store, target) = fixture("lazy-index");
        let targets = [target.clone()];

        let first = call_tool(&store, &targets, "freshness", &json!({}), false).unwrap();
        assert_eq!(first["repo"]["indexed"], false);

        let status = call_tool(&store, &targets, "status", &json!({}), false).unwrap();
        assert_eq!(status["repo"]["freshness"]["indexed"], true);
        assert!(
            status["repo"]["freshness"]["added"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        std::fs::write(
            target.root.join("lib.rs"),
            "pub fn original() {}\npub fn added() {}\n",
        )
        .unwrap();
        let stale = call_tool(&store, &targets, "freshness", &json!({}), false).unwrap();
        assert_eq!(stale["repo"]["modified"], json!(["lib.rs"]));

        let refreshed = call_tool(&store, &targets, "status", &json!({}), false).unwrap();
        assert!(
            refreshed["repo"]["freshness"]["modified"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let conn = db::open(&store).unwrap();
        let repo_id = index::repo_id_of(&conn, &target.root).unwrap().unwrap();
        let matches = index::grep(&conn, repo_id, &target.root, "added").unwrap();
        assert!(!matches.groups.is_empty());
    }

    #[test]
    fn initialize_responds_before_background_indexing_builds_and_refreshes() {
        let (_temp, store, target) = fixture("initialize-index");
        let targets = [target.clone()];

        let response = dispatch(
            &store,
            &targets,
            "initialize",
            &json!({}),
            false,
            &mut SessionStats::default(),
        )
        .unwrap();
        assert_eq!(response["serverInfo"]["name"], "panoptes");
        assert!(!store.exists(), "the handshake must not wait for SQLite");

        let mut worker = start_indexing(&store, &targets, false);
        finish_indexing(&mut worker);
        let conn = db::open(&store).unwrap();
        assert!(index::freshness(&conn, &target.root).unwrap().is_clean());
        drop(conn);

        std::fs::write(
            target.root.join("lib.rs"),
            "pub fn original() {}\npub fn refreshed() {}\n",
        )
        .unwrap();
        let mut worker = start_indexing(&store, &targets, false);
        finish_indexing(&mut worker);
        let conn = db::open(&store).unwrap();
        assert!(index::freshness(&conn, &target.root).unwrap().is_clean());
        let repo_id = index::repo_id_of(&conn, &target.root).unwrap().unwrap();
        let matches = index::grep(&conn, repo_id, &target.root, "refreshed").unwrap();
        assert!(!matches.groups.is_empty());
    }

    #[test]
    fn initialize_respects_no_refresh() {
        let (_temp, store, target) = fixture("initialize-no-refresh");

        dispatch(
            &store,
            std::slice::from_ref(&target),
            "initialize",
            &json!({}),
            true,
            &mut SessionStats::default(),
        )
        .unwrap();

        assert!(!store.exists());
    }

    #[test]
    fn initialize_without_targets_starts_idle_without_creating_a_store() {
        let temp = TempDir::new("initialize-idle");
        let store = temp.0.join("panoptes.db");

        let response = dispatch(
            &store,
            &[],
            "initialize",
            &json!({}),
            false,
            &mut SessionStats::default(),
        )
        .unwrap();

        assert_eq!(response["serverInfo"]["name"], "panoptes");
        assert!(
            response["instructions"]
                .as_str()
                .unwrap()
                .contains("started without indexing")
        );
        assert!(!store.exists());
    }

    #[test]
    fn tool_call_without_targets_explains_why_navigation_is_unavailable() {
        let temp = TempDir::new("tool-idle");
        let store = temp.0.join("panoptes.db");

        let problem = call_tool(&store, &[], "status", &json!({}), false)
            .unwrap_err()
            .to_string();

        assert!(problem.contains("current directory is not a Git repository"));
        assert!(!store.exists());
    }

    #[test]
    fn no_refresh_does_not_create_a_missing_index() {
        let (_temp, store, target) = fixture("no-refresh");
        let error = call_tool(
            &store,
            std::slice::from_ref(&target),
            "status",
            &json!({}),
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("automatic indexing is disabled"));
        let conn = db::open(&store).unwrap();
        assert!(!index::freshness(&conn, &target.root).unwrap().indexed);
    }

    #[test]
    fn invalid_tool_call_does_not_create_an_index() {
        let (_temp, store, target) = fixture("invalid-call");
        let error = call_tool(
            &store,
            std::slice::from_ref(&target),
            "unknown",
            &json!({}),
            false,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(error, "unknown tool: unknown");
        let conn = db::open(&store).unwrap();
        assert!(!index::freshness(&conn, &target.root).unwrap().indexed);
    }

    #[test]
    fn find_includes_bounded_source_by_default() {
        let (_temp, store, target) = fixture("find-source-default");
        let result = call_tool(
            &store,
            std::slice::from_ref(&target),
            "find",
            &json!({"query":"original"}),
            false,
        )
        .unwrap();
        assert_eq!(result["repo"]["hits"][0]["source"], "pub fn original() {}");
    }

    #[test]
    fn retrieval_reports_per_call_and_session_savings() {
        let (_temp, store, target) = fixture("savings");
        let mut source = String::from("pub fn original() {}\n");
        source.push_str(&"// representative source line for baseline sizing\n".repeat(400));
        std::fs::write(target.root.join("lib.rs"), source).unwrap();
        let targets = [target];
        let mut session = SessionStats::default();

        let first = dispatch(
            &store,
            &targets,
            "tools/call",
            &json!({"name":"find", "arguments":{"query":"original"}}),
            false,
            &mut session,
        )
        .unwrap();
        let first_savings = &first["structuredContent"]["panoptesSavings"];
        assert!(first_savings["estimatedTokensSaved"].as_u64().unwrap() > 0);
        assert_eq!(first_savings["matchedFiles"], 1);
        assert_eq!(first_savings["sessionCallsWithSavings"], 1);

        let second = dispatch(
            &store,
            &targets,
            "tools/call",
            &json!({"name":"skeleton", "arguments":{"file":"lib.rs"}}),
            false,
            &mut session,
        )
        .unwrap();
        let second_savings = &second["structuredContent"]["panoptesSavings"];
        assert_eq!(second_savings["sessionCallsWithSavings"], 2);
        assert!(
            second_savings["sessionEstimatedTokensSaved"]
                .as_u64()
                .unwrap()
                > first_savings["sessionEstimatedTokensSaved"]
                    .as_u64()
                    .unwrap()
        );
        assert!(
            second["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("sessionEstimatedTokensSaved")
        );
        assert_eq!(
            second_savings["sessionSavingsDisplay"].as_str().unwrap(),
            session_savings_display(
                second_savings["sessionEstimatedTokensSaved"]
                    .as_u64()
                    .unwrap()
            )
        );
    }

    #[test]
    fn savings_display_uses_panoptes_mark_and_grouped_total() {
        assert_eq!(
            session_savings_display(1_231_578),
            "ꙮ Estimated tokens saved for this session: 1,231,578"
        );
    }
}
