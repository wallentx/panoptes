#!/usr/bin/env python3
"""Agent benchmark that drives a model endpoint directly over chat completions.

Like scripts/benchmark-agent.sh, this runs the read-only coding questions in a
task file against a fresh checkout, first without Panoptes and then with it.
Instead of a coding-agent client, it implements a minimal agent loop with its
own read-only tools (read_file, grep_files, glob_files) and, for the Panoptes
arm, the Panoptes MCP server's tools over stdio. Token usage comes straight
from the endpoint's usage fields.

The source repository is cloned into scratch storage for every run and detached
at the initially verified HEAD, so all arms use one committed snapshot. Bundled
PocketBase tasks require a matching GitHub origin and committed server symbols;
--tasks selects custom tasks for another corpus. Actual revision and configured
pin agreement are recorded. Panoptes stores are per-run scratch databases.

Events include request/schema/message sizes, endpoint and tool elapsed times,
and explicit truncation flags for retained tool-output previews. Full tool output
sidecars are stored outside the agent worktree, with SHA-256 and relative paths
in JSONL. The rubric columns measure keyword presence only, not accuracy.

Requirements: Python 3.9+, Git, the Panoptes binary, an OpenAI-compatible
chat-completions endpoint, and jq is NOT required.

Usage:
  python3 scripts/benchmark-agent-llm.py \
    --source /path/to/pocketbase \
    --output "$TMPDIR/panoptes-agent-benchmark" \
    [--tasks FILE] [--task ID] [--trials N] [--model MODEL] \
    [--binary PATH] [--base-url URL] [--api-key KEY] [--max-turns N]

Credentials: --base-url/--api-key, or OPENAI_BASE_URL/OPENAI_API_KEY in the
environment, or ~/.ssh/tokens/glm53 (OPENAI_BASE_URL=... and OPENAI_API_KEY=...
lines), matching the codex-glm53 bridge convention.

Pricing inputs match benchmark-agent.sh and can be overridden with
PANOPTES_AGENT_INPUT_USD_PER_MTOK, PANOPTES_AGENT_CACHED_INPUT_USD_PER_MTOK,
and PANOPTES_AGENT_OUTPUT_USD_PER_MTOK.
"""

import argparse
import csv
import fnmatch
import glob
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import urllib.parse

DEFAULT_MODEL = "zai-org/GLM-5.3"
DEFAULT_TASKS = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                             "bench", "agent-tasks", "pocketbase.tsv")
DEFAULT_MAX_TURNS = 60
REQUEST_TIMEOUT_SEC = 300
TRANSPORT_ATTEMPTS = 3
TRANSPORT_BACKOFF_SEC = [2.0, 8.0]
MAX_GREP_MATCHES = 200
MAX_GREP_LINE = 2000
MAX_READ_LINES = 2000
MAX_READ_LINE_CHARS = 2000
MAX_GLOB_MATCHES = 200
RUBRIC_METHOD = "keyword_presence_not_correctness"

VERBOSE = False


def progress(message):
    if VERBOSE:
        print(message, file=sys.stderr, flush=True)

GUIDANCE = (
    "If Panoptes MCP tools are available, use them before shell search or whole-file reads. "
    "Use find for ranked context, grep for exhaustive matches, callers for dependency paths, "
    "skeleton for a file API, and map for orientation. For this scoped task, start with find "
    "or grep instead of map, use at most three Panoptes calls, and do not repeat find to "
    "reconstruct individual files. If they are unavailable, use the built-in read-only tools. "
)


SYSTEM_PROMPT = (
    "You are a coding agent answering a read-only question about the repository checked out at "
    "{worktree}. You cannot modify files. Investigate with your tools, then answer precisely with "
    "concrete evidence: file paths, line numbers, and symbol names. If you are unsure, say so.\n\n"
    + GUIDANCE
    + "\n\nAvailable tools: read_file, grep_files, glob_files{tools_sentence}."
)


def load_credentials(args):
    # Each option has its own precedence; a partial CLI override must not be
    # discarded just because the environment supplies a complete pair.
    base = args.base_url or os.environ.get("OPENAI_BASE_URL")
    key = args.api_key or os.environ.get("OPENAI_API_KEY")
    if base and key:
        return base.rstrip("/"), key
    token_file = os.path.expanduser("~/.ssh/tokens/glm53")
    values = {}
    try:
        with open(token_file, "r", encoding="utf-8") as handle:
            for line in handle.read().splitlines():
                match = re.fullmatch(r"\s*(?:export\s+)?(OPENAI_API_KEY|OPENAI_BASE_URL)\s*=\s*(.*)", line)
                if match:
                    value = match.group(2).strip()
                    if len(value) >= 2 and value[0] == value[-1] and value[0] in "\"'":
                        value = value[1:-1]
                    values[match.group(1)] = value
    except OSError:
        pass
    base = base or values.get("OPENAI_BASE_URL")
    key = key or values.get("OPENAI_API_KEY")
    if not base or not key:
        sys.exit("no endpoint credentials: pass --base-url/--api-key, set OPENAI_BASE_URL/OPENAI_API_KEY, "
                  "or create ~/.ssh/tokens/glm53")
    return base.rstrip("/"), key


POCKETBASE_ANCHORS = {
    "apis/base.go": ("NewRouter",),
    "apis/record_auth.go": ("bindRecordAuthApi",),
    "apis/record_auth_with_password.go": ("recordAuthWithPassword",),
    "apis/record_auth_with_otp.go": ("recordAuthWithOTP",),
    "apis/record_auth_with_oauth2.go": ("recordAuthWithOAuth2",),
    "apis/record_helpers.go": ("recordAuthResponse", "checkMFA", "authAlert"),
}


def git_environment():
    # A caller's GIT_DIR/GIT_WORK_TREE must not redirect corpus operations.
    return {key: value for key, value in os.environ.items() if not key.startswith("GIT_")}


def git_output(source, *arguments):
    result = subprocess.run(["git", "-C", source, *arguments], env=git_environment(),
                            check=True, capture_output=True, text=True)
    return result.stdout.strip()


def canonical_github_repository(remote):
    remote = remote.strip()
    if remote.startswith("git@github.com:"):
        path = remote[len("git@github.com:"):]
    else:
        parsed = urllib.parse.urlsplit(remote)
        if parsed.scheme not in ("https", "ssh") or parsed.hostname != "github.com" or parsed.query or parsed.fragment:
            return None
        path = parsed.path.lstrip("/")
    path = path.rstrip("/")
    if path.lower().endswith(".git"):
        path = path[:-4]
    return path.lower() if re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", path) else None


def validate_source(source, tasks):
    """Validate the committed material that clone_worktree will actually expose."""
    source = os.path.realpath(source)
    try:
        if git_output(source, "rev-parse", "--is-inside-work-tree") != "true":
            raise ValueError("source must be a Git worktree")
        if os.path.realpath(git_output(source, "rev-parse", "--show-toplevel")) != source:
            raise ValueError("--source must name the root of its Git worktree")
        revision = git_output(source, "rev-parse", "HEAD")
        if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", revision):
            raise ValueError("source has no valid committed HEAD")
        try:
            remote = git_output(source, "remote", "get-url", "origin")
        except subprocess.CalledProcessError:
            remote = ""
        repository = canonical_github_repository(remote)
        provenance = {"source_revision": revision,
                      "source_repository": repository or "custom/local repository",
                      "source_validation": "custom tasks: Git root and revision verified"}
        if os.path.realpath(tasks) != os.path.realpath(DEFAULT_TASKS):
            return provenance
        if repository != "pocketbase/pocketbase":
            raise ValueError("bundled PocketBase tasks require an origin identifying github.com/pocketbase/pocketbase; the corpus may be mislabeled")
        tracked = set(git_output(source, "ls-tree", "-r", "--name-only", revision).splitlines())
        leakage = {"bench/agent-tasks/pocketbase.tsv", "bench/corpora/pocketbase.conf"} & tracked
        if leakage:
            raise ValueError("bundled benchmark task/config metadata must not be visible inside the corpus")
        for path, symbols in POCKETBASE_ANCHORS.items():
            content = git_output(source, "show", revision + ":" + path)
            for symbol in symbols:
                if not re.search(r"\b" + re.escape(symbol) + r"\b", content):
                    raise ValueError("PocketBase corpus is missing required task evidence: %s in %s" % (symbol, path))
        config = os.path.join(os.path.dirname(DEFAULT_TASKS), "..", "corpora", "pocketbase.conf")
        with open(config, encoding="utf-8") as handle:
            expected = next((line.strip().split("=", 1)[1] for line in handle
                             if line.startswith("revision=")), "")
        provenance.update({"source_validation": "PocketBase origin, committed task evidence, and metadata isolation verified",
                           "corpus_expected_revision": expected,
                           "source_matches_configured_revision": str(revision == expected).lower()})
        if expected and revision != expected:
            print("note: explicit --source HEAD differs from the configured corpus revision; recording the actual committed HEAD", file=sys.stderr)
        return provenance
    except (OSError, subprocess.CalledProcessError) as exc:
        raise ValueError("cannot validate committed corpus: %s" % type(exc).__name__) from exc


def chat_payload(model, messages, tools, reasoning_effort=None):
    payload = {"model": model, "messages": messages, "stream": False}
    if tools:
        payload["tools"] = tools
    if reasoning_effort:
        payload["reasoning_effort"] = reasoning_effort
    return payload


def request_sizes(model, messages, tools, reasoning_effort=None):
    body = json.dumps(chat_payload(model, messages, tools, reasoning_effort))
    return {"message_chars": len(json.dumps(messages)),
            "schema_chars": len(json.dumps(tools)) if tools else 0,
            "request_chars": len(body), "request_bytes": len(body.encode("utf-8"))}


class Endpoint:
    def __init__(self, base_url, api_key, model):
        self.base_url = base_url
        self.api_key = api_key
        self.model = model

    def chat(self, messages, tools, reasoning_effort=None):
        body = json.dumps(chat_payload(self.model, messages, tools, reasoning_effort)).encode("utf-8")
        last_error = None
        for attempt in range(TRANSPORT_ATTEMPTS):
            request = urllib.request.Request(
                self.base_url + "/chat/completions", data=body, method="POST",
                headers={"Authorization": "Bearer " + self.api_key,
                          "Content-Type": "application/json"})
            try:
                with urllib.request.urlopen(request, timeout=REQUEST_TIMEOUT_SEC) as response:
                    return json.load(response)
            except (urllib.error.URLError, urllib.error.HTTPError, TimeoutError, ConnectionError, OSError) as exc:
                last_error = exc
                if attempt + 1 < TRANSPORT_ATTEMPTS:
                    time.sleep(TRANSPORT_BACKOFF_SEC[min(attempt, len(TRANSPORT_BACKOFF_SEC) - 1)])
        raise RuntimeError("endpoint failed after %d attempts: %s" % (TRANSPORT_ATTEMPTS, last_error))


class PanoptesMcp:
    def __init__(self, binary, store, worktree, compact=False):
        self.prefix = "panoptes_"
        command = [binary, "--store", store, "mcp", worktree]
        if compact:
            command.append("--compact")
        self.proc = subprocess.Popen(
            command,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
            text=True, encoding="utf-8", bufsize=1)
        self.next_id = 1
        self.pending = {}
        self._initialize()

    def _send(self, method, params=None, notification=False):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        if notification:
            self.proc.stdin.write(json.dumps(message) + "\n")
            self.proc.stdin.flush()
            return None
        message["id"] = self.next_id
        self.next_id += 1
        self.proc.stdin.write(json.dumps(message) + "\n")
        self.proc.stdin.flush()
        while True:
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("panoptes mcp closed stdout")
            response = json.loads(line)
            if "id" not in response:
                continue
            if response["id"] != message["id"]:
                continue
            if "error" in response:
                raise RuntimeError("panoptes mcp error: %s" % response["error"])
            return response.get("result")

    def _initialize(self):
        self._send("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                   "clientInfo": {"name": "panoptes-agent-bench", "version": "1"}})
        self._send("notifications/initialized", notification=True)

    def list_tools(self):
        result = self._send("tools/list")
        tools = []
        for tool in result.get("tools", []):
            name = tool.get("name")
            if not name:
                continue
            tools.append({"type": "function", "function": {
                "name": self.prefix + name,
                "description": tool.get("description") or name,
                "parameters": tool.get("inputSchema") or {"type": "object", "properties": {}},
            }})
        return tools

    def call(self, name, arguments):
        result = self._send("tools/call", {"name": name, "arguments": arguments})
        parts = result.get("content", []) if isinstance(result, dict) else []
        text = "\n".join(part.get("text", "") for part in parts if isinstance(part, dict))
        failed = isinstance(result, dict) and result.get("isError") is True
        return text, failed

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()


def safe_path(worktree, relative):
    root = os.path.realpath(worktree)
    candidate = os.path.realpath(os.path.join(worktree, relative))
    if candidate != root and not candidate.startswith(root + os.sep):
        raise ValueError("path escapes the worktree: %s" % relative)
    if ".git" in os.path.relpath(candidate, root).split(os.sep):
        raise ValueError("path enters Git metadata: %s" % relative)
    return candidate


def tool_read_file(worktree, arguments, events):
    path = arguments.get("path")
    if not isinstance(path, str) or not path:
        return "error: path is required"
    offset = arguments.get("offset", 1)
    limit = arguments.get("limit", MAX_READ_LINES)
    try:
        offset = max(1, int(offset))
        limit = max(1, int(limit))
    except (TypeError, ValueError):
        return "error: offset and limit must be integers"
    try:
        target = safe_path(worktree, path)
        with open(target, "r", encoding="utf-8", errors="replace") as handle:
            lines = handle.readlines()
    except (OSError, ValueError) as exc:
        return "error: %s" % exc
    selected = lines[offset - 1:offset - 1 + limit]
    rendered = []
    for index, line in enumerate(selected, start=offset):
        line = line.rstrip("\n")
        if len(line) > MAX_READ_LINE_CHARS:
            line = line[:MAX_READ_LINE_CHARS] + "…"
        rendered.append("%d: %s" % (index, line))
    if not rendered:
        return "(empty range: file has %d lines, offset %d)" % (len(lines), offset)
    return "\n".join(rendered)


def iter_files(worktree, subpath):
    root = safe_path(worktree, subpath)
    if os.path.isfile(root):
        yield os.path.join(worktree, subpath), root
        return
    for current, dirnames, filenames in os.walk(root, followlinks=False):
        dirnames[:] = sorted(d for d in dirnames if d != ".git")
        for filename in sorted(filenames):
            discovered = os.path.join(current, filename)
            try:
                # A safe search root does not establish containment for each
                # symlinked file discovered below it. Open only checked targets.
                resolved = safe_path(worktree, os.path.relpath(discovered, worktree))
                if not os.path.isfile(resolved):
                    continue
            except (OSError, ValueError):
                continue
            display = os.path.join(worktree, subpath, os.path.relpath(discovered, root))
            yield display, resolved


def tool_grep_files(worktree, arguments, events):
    # Match the canonical paths produced by traversal (e.g. macOS /private/var).
    worktree = os.path.realpath(worktree)
    pattern = arguments.get("pattern")
    if not isinstance(pattern, str) or not pattern:
        return "error: pattern is required"
    subpath = arguments.get("path") or "."
    include = arguments.get("include")
    literal = bool(arguments.get("literal", False))
    try:
        matcher = re.compile(re.escape(pattern) if literal else pattern)
    except re.error as exc:
        return "error: invalid pattern: %s" % exc
    try:
        files = list(iter_files(worktree, subpath))
    except (OSError, ValueError) as exc:
        return "error: %s" % exc
    matches = []
    for filepath, resolved in files:
        if include and not fnmatch.fnmatch(os.path.basename(filepath), include):
            continue
        try:
            with open(resolved, "r", encoding="utf-8", errors="replace") as handle:
                if "\0" in handle.read(4096):
                    continue
                handle.seek(0)
                for lineno, line in enumerate(handle, start=1):
                    line = line.rstrip("\n")
                    if matcher.search(line):
                        relative = os.path.relpath(filepath, worktree)
                        if len(line) > MAX_GREP_LINE:
                            line = line[:MAX_GREP_LINE] + "…"
                        matches.append("%s:%d:%s" % (relative, lineno, line))
                        if len(matches) >= MAX_GREP_MATCHES:
                            matches.append("(stopped at %d matches)" % MAX_GREP_MATCHES)
                            return "\n".join(matches)
        except OSError:
            continue
    return "\n".join(matches) if matches else "(no matches)"


def tool_glob_files(worktree, arguments, events):
    worktree = os.path.realpath(worktree)
    pattern = arguments.get("pattern")
    if not isinstance(pattern, str) or not pattern:
        return "error: pattern is required"
    subpath = arguments.get("path") or "."
    matches = []
    try:
        for display, _resolved in iter_files(worktree, subpath):
            relative = os.path.relpath(display, worktree)
            if fnmatch.fnmatch(relative, pattern):
                matches.append(relative)
                if len(matches) >= MAX_GLOB_MATCHES:
                    matches.append("(stopped at %d matches)" % MAX_GLOB_MATCHES)
                    return "\n".join(matches)
    except (OSError, ValueError) as exc:
        return "error: %s" % exc
    return "\n".join(matches) if matches else "(no matches)"


def build_builtin_tools():
    return [
        {"type": "function", "function": {
            "name": "read_file",
            "description": "Read a file from the worktree. Returns numbered lines.",
            "parameters": {"type": "object", "properties": {
                "path": {"type": "string", "description": "Worktree-relative file path"},
                "offset": {"type": "integer", "description": "First line to return (1-based, default 1)"},
                "limit": {"type": "integer", "description": "Maximum lines to return (default %d)" % MAX_READ_LINES},
            }, "required": ["path"]}}},
        {"type": "function", "function": {
            "name": "grep_files",
            "description": "Search file contents in the worktree. Returns path:line:text matches.",
            "parameters": {"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Regular expression (or literal text when literal is true)"},
                "path": {"type": "string", "description": "Worktree-relative directory or file to search (default: worktree root)"},
                "include": {"type": "string", "description": "Optional glob for file names, for example *.go"},
                "literal": {"type": "boolean", "description": "Match the pattern literally instead of as a regex"},
            }, "required": ["pattern"]}}},
        {"type": "function", "function": {
            "name": "glob_files",
            "description": "List worktree-relative paths matching a glob pattern.",
            "parameters": {"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Glob pattern such as **/*.go"},
                "path": {"type": "string", "description": "Worktree-relative directory to search (default: worktree root)"},
            }, "required": ["pattern"]}}},
    ]



def create_tool_output_store(events_path, worktree):
    """Keep complete output artifacts outside the agent's readable worktree."""
    events_path = os.path.realpath(events_path)
    worktree = os.path.realpath(worktree)
    if os.path.commonpath([events_path, worktree]) == worktree:
        raise ValueError("benchmark evidence must be stored outside the agent worktree")
    event_directory = os.path.dirname(events_path)
    directory = os.path.join(event_directory, "tool-outputs")
    os.makedirs(directory, exist_ok=True)
    if os.path.realpath(directory) != directory:
        raise ValueError("tool-output artifact directory must not be a symlink")
    if os.path.commonpath([directory, worktree]) == worktree:
        raise ValueError("tool-output artifacts must remain outside the agent worktree")
    return tempfile.mkdtemp(prefix="run-", dir=directory), event_directory


def save_tool_output(directory, event_directory, sequence, output):
    # Only an internal monotonically increasing number forms the filename.
    # Tool names, model arguments, credentials, and source paths are never used.
    name = "%06d.txt" % sequence
    path = os.path.join(directory, name)
    relative = os.path.relpath(path, event_directory)
    if os.path.isabs(relative) or ".." in relative.split(os.sep):
        raise ValueError("tool-output artifact escaped its evidence directory")
    data = output.encode("utf-8")
    with open(path, "xb") as handle:
        handle.write(data)
    return {"output_path": relative.replace(os.sep, "/"),
            "output_sha256": hashlib.sha256(data).hexdigest(),
            "output_bytes": len(data)}

def run_one(endpoint, task, prompt, arm, trial, effort, worktree, mcp, events_path, answer_path):
    started = time.monotonic()
    metrics = {
        "exit_code": 0, "input_tokens": 0, "cached_input_tokens": 0, "output_tokens": 0,
        "reasoning_output_tokens": 0, "tool_calls": 0, "panoptes_calls": 0, "mcp_failures": 0,
    }
    tool_output_directory, event_directory = create_tool_output_store(events_path, worktree)
    events_file = open(events_path, "w", encoding="utf-8")
    run_label = "%s/%s/trial%d%s" % (task, arm, trial, "/" + effort if effort else "")

    def record(event):
        events_file.write(json.dumps(event, ensure_ascii=False) + "\n")
        events_file.flush()

    tools = build_builtin_tools()
    tools_sentence = ""
    if arm == "panoptes" and mcp is not None:
        tools.extend(mcp.list_tools())
        tools_sentence = ", plus the Panoptes MCP tools (names prefixed with %s)" % mcp.prefix
    progress("[%s] starting; %d tools available" % (run_label, len(tools)))
    system = SYSTEM_PROMPT.format(worktree=worktree, tools_sentence=tools_sentence)
    messages = [
        {"role": "system", "content": system},
        {"role": "user", "content": prompt},
    ]
    record({"type": "run.started", "arm": arm, "task": task, "trial": trial, "effort": effort,
             "model": endpoint.model, "tool_names": [t["function"]["name"] for t in tools],
             "rubric_method": RUBRIC_METHOD})

    answer = ""
    try:
        for _turn in range(args_max_turns):
            record({"type": "request", "turn": _turn + 1,
                    **request_sizes(endpoint.model, messages, tools, effort)})
            request_started = time.monotonic()
            response = endpoint.chat(messages, tools, reasoning_effort=effort)
            endpoint_elapsed_ms = round((time.monotonic() - request_started) * 1000, 3)
            usage = response.get("usage") or {}
            metrics["input_tokens"] += usage.get("prompt_tokens") or 0
            metrics["cached_input_tokens"] += (usage.get("prompt_tokens_details") or {}).get("cached_tokens") or 0
            metrics["output_tokens"] += usage.get("completion_tokens") or 0
            details = usage.get("completion_tokens_details") or {}
            metrics["reasoning_output_tokens"] += details.get("reasoning_tokens") or usage.get("reasoning_tokens") or 0
            choice = (response.get("choices") or [{}])[0]
            message = choice.get("message") or {}
            usage_note = "in=%d cached=%d out=%d reason=%d" % (
                usage.get("prompt_tokens") or 0,
                (usage.get("prompt_tokens_details") or {}).get("cached_tokens") or 0,
                usage.get("completion_tokens") or 0,
                details.get("reasoning_tokens") or usage.get("reasoning_tokens") or 0)
            reasoning = message.get("reasoning_content") or ""
            if reasoning:
                record({"type": "reasoning", "text": reasoning})
            tool_calls = message.get("tool_calls") or []
            turn_tools = [call.get("function", {}).get("name") for call in tool_calls]
            if turn_tools:
                progress("[%s] turn %d: %s | %s" % (run_label, _turn + 1, usage_note, ",".join(turn_tools)))
            content = message.get("content")
            if not turn_tools and content:
                progress("[%s] turn %d: %s | final answer (%d chars)" % (run_label, _turn + 1, usage_note, len(content)))
            record({"type": "assistant", "content": content,
                    "tool_calls": [{"name": c.get("function", {}).get("name"),
                                    "arguments": c.get("function", {}).get("arguments")}
                                   for c in tool_calls],
                    "usage": usage, "endpoint_elapsed_ms": endpoint_elapsed_ms,
                    "finish_reason": choice.get("finish_reason")})
            replay = {"role": "assistant", "content": content}
            if tool_calls:
                replay["tool_calls"] = [
                    {"id": call.get("id"), "type": "function",
                     "function": {"name": call.get("function", {}).get("name"),
                                  "arguments": call.get("function", {}).get("arguments") or "{}"}}
                    for call in tool_calls]
            messages.append(replay)
            finish_reason = choice.get("finish_reason")
            if tool_calls:
                if finish_reason not in ("tool_calls", "stop"):
                    raise RuntimeError("incomplete tool-call response: finish_reason=%r" % finish_reason)
            else:
                if finish_reason != "stop":
                    raise RuntimeError("incomplete final response: finish_reason=%r" % finish_reason)
                if not isinstance(content, str) or not content.strip():
                    raise RuntimeError("empty final response")
                answer = content.strip()
                break
            for call in tool_calls:
                tool_started = time.monotonic()
                function = call.get("function", {})
                name = function.get("name") or ""
                raw_arguments = function.get("arguments") or "{}"
                metrics["tool_calls"] += 1
                try:
                    arguments = json.loads(raw_arguments) if raw_arguments.strip() else {}
                    if not isinstance(arguments, dict):
                        raise ValueError("arguments must be a JSON object")
                except ValueError as exc:
                    output_text, failed = "error: invalid tool arguments: %s" % exc, True
                else:
                    if name == "read_file":
                        output_text, failed = tool_read_file(worktree, arguments, events_file), False
                    elif name == "grep_files":
                        output_text, failed = tool_grep_files(worktree, arguments, events_file), False
                    elif name == "glob_files":
                        output_text, failed = tool_glob_files(worktree, arguments, events_file), False
                    elif mcp is not None and name.startswith(mcp.prefix):
                        try:
                            output_text, failed = mcp.call(name[len(mcp.prefix):], arguments)
                            metrics["panoptes_calls"] += 0 if failed else 1
                            if failed:
                                metrics["mcp_failures"] += 1
                        except RuntimeError as exc:
                            output_text, failed = "error: %s" % exc, True
                            metrics["mcp_failures"] += 1
                    else:
                        output_text, failed = "error: unknown tool: %s" % name, True
                elapsed_ms = round((time.monotonic() - tool_started) * 1000, 3)
                artifact = save_tool_output(tool_output_directory, event_directory,
                                            metrics["tool_calls"], output_text)
                record({"type": "tool_result", "name": name, "failed": failed,
                         "output_chars": len(output_text), "output": output_text[:4000],
                         "output_truncated": len(output_text) > 4000,
                         "elapsed_ms": elapsed_ms, **artifact})
                progress("[%s]   -> %s (%d chars%s)" % (run_label, name, len(output_text), ", FAILED" if failed else ""))
                messages.append({"role": "tool", "tool_call_id": call.get("id"),
                                 "content": output_text})
        else:
            metrics["exit_code"] = 2
            answer = answer or ""
            record({"type": "run.max_turns"})
    except Exception as exc:
        metrics["exit_code"] = 1
        record({"type": "run.error", "error": str(exc)})
    finally:
        if mcp is not None:
            mcp.close()
        record({"type": "run.finished", "answer": answer, "metrics": metrics})
        events_file.close()
    with open(answer_path, "w", encoding="utf-8") as handle:
        handle.write(answer)
    metrics["wall_ms"] = int((time.monotonic() - started) * 1000)
    return metrics, answer



def print_quick_results(results_path):
    """Print paired completed rows using the same cost/rubric values as results.tsv."""
    pairs = {}
    with open(results_path, encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            if row["exit_code"] != "0" or row["arm"] not in ("baseline", "panoptes"):
                continue
            key = (row["task"], row["trial"], row["effort"] or "default")
            pair = pairs.setdefault(key, {})
            if row["arm"] in pair:
                raise ValueError("duplicate quick comparison row: %s" % (key,))
            pair[row["arm"]] = row
    for (task, trial, effort), pair in pairs.items():
        if set(pair) != {"baseline", "panoptes"}:
            continue
        base, pan = pair["baseline"], pair["panoptes"]
        print("  %s / effort %s / trial %s:" % (task, effort, trial))
        for label, field, conversion, formatter in (
                ("cost", "estimated_cost_usd", float, "$%.4f"),
                ("input", "input_tokens", int, "%d"),
                ("uncached", "uncached_input_tokens", int, "%d")):
            before, after = conversion(base[field]), conversion(pan[field])
            change = "%+.1f%%" % (100.0 * (after / before - 1)) if before else "n/a"
            print("  %s %s vs %s (%s)" % (label, formatter % before, formatter % after, change))
        print("  tool_calls %s vs %s" % (base["tool_calls"], pan["tool_calls"]))
        print("  keyword coverage (not accuracy) %s/%s vs %s/%s" % (base["rubric_hits"], base["rubric_total"],
                                             pan["rubric_hits"], pan["rubric_total"]))

def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--source", required=True, help="pinned corpus checkout")
    parser.add_argument("--output", help="output directory (default: fresh directory under $TMPDIR)")
    parser.add_argument("--tasks", default=DEFAULT_TASKS, help="task file (task<TAB>prompt<TAB>required_terms); rubric columns measure keyword presence, not accuracy")
    parser.add_argument("--task", help="run only this task id (smoke test)")
    parser.add_argument("--trials", type=int, default=1, help="trials per task (default 1)")
    parser.add_argument("--model", default=DEFAULT_MODEL, help="chat-completions model id (default %(default)s)")
    parser.add_argument("--binary", default=os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                                                        "target", "release", "panoptes"),
                        help="panoptes binary (default: target/release/panoptes; PANOPTES_BENCH_BIN env)")
    parser.add_argument("--base-url", help="chat-completions base URL (default: credentials file/env)")
    parser.add_argument("--api-key", help="endpoint API key (default: credentials file/env)")
    parser.add_argument("--max-turns", type=int, default=DEFAULT_MAX_TURNS,
                        help="maximum assistant turns per run (default %(default)s)")
    parser.add_argument("--reasoning-effort", default="",
                        help="comma-separated reasoning-effort values to sweep (for example low,high,max; default: endpoint default)")
    parser.add_argument("--mcp-compact", action="store_true",
                        help="use Panoptes mcp --compact for the Panoptes arm (default: legacy JSON text)")
    parser.add_argument("--quick", action="store_true",
                        help="run only the auth-routing task, 1 trial per arm, low effort only, "
                             "and print an inline find-bloat comparison (iterating on panoptes fixes)")
    parser.add_argument("--verbose", dest="verbose", action="store_true", default=None,
                        help="stream per-turn progress to stderr (default: on when stderr is a tty)")
    parser.add_argument("--quiet", dest="verbose", action="store_false",
                        help="suppress per-turn progress output")
    args = parser.parse_args()

    global args_max_turns, VERBOSE
    args_max_turns = args.max_turns
    VERBOSE = args.verbose
    if args.verbose is None:
        args.verbose = sys.stderr.isatty()

    if os.environ.get("PANOPTES_BENCH_BIN"):
        args.binary = os.environ["PANOPTES_BENCH_BIN"]
    if not os.path.isfile(args.binary) or not os.access(args.binary, os.X_OK):
        sys.exit("panoptes binary not found or not executable: %s" % args.binary)
    try:
        provenance = validate_source(args.source, args.tasks)
    except ValueError as exc:
        parser.error(str(exc))
    source_revision = provenance["source_revision"]
    if args.trials < 1:
        sys.exit("--trials must be a positive integer")

    if args.quick:
        if not args.task:
            args.task = "auth-routing"
        args.trials = 1
        if not args.reasoning_effort:
            args.reasoning_effort = "low"

    efforts = []
    for value in (part.strip() for part in args.reasoning_effort.split(",")):
        if value and value not in efforts:
            efforts.append(value)
    if not efforts:
        efforts = [""]

    base_url, api_key = load_credentials(args)
    endpoint = Endpoint(base_url, api_key, args.model)

    source = os.path.realpath(args.source)
    tasks = []
    with open(args.tasks, "r", encoding="utf-8") as handle:
        for line in handle:
            line = line.rstrip("\n")
            if not line or line.startswith("task\t"):
                continue
            parts = line.split("\t")
            if len(parts) < 3:
                continue
            name, prompt, required_terms = parts[0], parts[1], parts[2]
            if args.task and name != args.task:
                continue
            tasks.append((name, prompt, [t for t in required_terms.split("|") if t]))

    if not tasks:
        sys.exit("no tasks matched: %s" % (args.task or "task file was empty"))

    input_rate = float(os.environ.get("PANOPTES_AGENT_INPUT_USD_PER_MTOK", "2.00"))
    cached_rate = float(os.environ.get("PANOPTES_AGENT_CACHED_INPUT_USD_PER_MTOK", "0.20"))
    output_rate = float(os.environ.get("PANOPTES_AGENT_OUTPUT_USD_PER_MTOK", "12.00"))

    tmp_base = os.environ.get("TMPDIR") or tempfile.gettempdir()
    output = args.output or os.path.join(
        tmp_base, "panoptes-agent-llm-benchmark-%s-%d" % (time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()), os.getpid()))
    os.makedirs(os.path.join(output, "runs"), exist_ok=True)
    scratch = tempfile.mkdtemp(prefix="panoptes-agent-llm.", dir=tmp_base)
    benchmark_failed = 0

    header = ("task\tarm\ttrial\teffort\texit_code\twall_ms\tinput_tokens\tcached_input_tokens\tuncached_input_tokens\t"
               "output_tokens\testimated_cost_usd\treasoning_output_tokens\ttool_calls\tpanoptes_calls\t"
               "mcp_failures\trubric_hits\trubric_total\n")
    results_path = os.path.join(output, "results.tsv")
    with open(results_path, "w", encoding="utf-8") as handle:
        handle.write(header)

    def clone_worktree(name):
        worktree = os.path.join(scratch, name)
        subprocess.run(["git", "clone", "--quiet", "--no-hardlinks", source, worktree],
                       env=git_environment(), check=True, capture_output=True)
        subprocess.run(["git", "-C", worktree, "checkout", "--quiet", "--detach", source_revision],
                       env=git_environment(), check=True, capture_output=True)
        return worktree

    try:
        for effort in efforts:
            for trial in range(1, args.trials + 1):
                arms = ["baseline", "panoptes"] if trial % 2 == 1 else ["panoptes", "baseline"]
                for name, prompt, required_terms in tasks:
                    for arm in arms:
                        run_id = "%s-%s-%d%s" % (name, arm, trial, "-" + effort if effort else "")
                        print("running %s trial %d effort %s (%s)" % (name, trial, effort or "default", arm), flush=True)
                        worktree = clone_worktree(run_id)
                        events_path = os.path.join(output, "runs", run_id + ".jsonl")
                        answer_path = os.path.join(output, "runs", run_id + ".md")
                        mcp = None
                        if arm == "panoptes":
                            mcp = PanoptesMcp(args.binary, os.path.join(scratch, run_id + ".db"), worktree, compact=args.mcp_compact)
                        metrics, answer = run_one(endpoint, name, prompt, arm, trial, effort, worktree, mcp,
                                                 events_path, answer_path)
                        if metrics["exit_code"] != 0:
                            benchmark_failed = 1
                        rubric_hits = sum(1 for term in required_terms if term.lower() in answer.lower())
                        uncached = metrics["input_tokens"] - metrics["cached_input_tokens"]
                        cost = (uncached * input_rate + metrics["cached_input_tokens"] * cached_rate
                                + metrics["output_tokens"] * output_rate) / 1_000_000
                        row = [name, arm, str(trial), effort, str(metrics["exit_code"]), str(metrics["wall_ms"]),
                               str(metrics["input_tokens"]), str(metrics["cached_input_tokens"]), str(uncached),
                               str(metrics["output_tokens"]), "%.6f" % cost,
                               str(metrics["reasoning_output_tokens"]), str(metrics["tool_calls"]),
                               str(metrics["panoptes_calls"]), str(metrics["mcp_failures"]),
                               str(rubric_hits), str(len(required_terms))]
                        with open(results_path, "a", encoding="utf-8") as handle:
                            handle.write("\t".join(row) + "\n")
                        shutil.rmtree(worktree, ignore_errors=True)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)

    with open(results_path, "r", encoding="utf-8") as handle:
        lines = [line.split("\t") for line in handle.read().splitlines()[1:] if line]
    summary_path = os.path.join(output, "summary.tsv")
    totals = {}
    for parts in lines:
        key = (parts[3], parts[1])
        entry = totals.setdefault(key, {"runs": 0, "successful": 0, "cost": 0.0, "input": 0, "uncached": 0,
                                        "output": 0, "tools": 0, "wall": 0, "hits": 0, "rubric": 0})
        entry["runs"] += 1
        if parts[4] == "0":
            entry["successful"] += 1
        entry["cost"] += float(parts[10])
        entry["input"] += int(parts[6])
        entry["uncached"] += int(parts[8])
        entry["output"] += int(parts[9])
        entry["tools"] += int(parts[12])
        entry["wall"] += int(parts[5])
        entry["hits"] += int(parts[15])
        entry["rubric"] += int(parts[16])
    with open(summary_path, "w", encoding="utf-8") as handle:
        handle.write("effort\tarm\truns\tsuccessful\testimated_cost_usd\tinput_tokens\tuncached_input_tokens\t"
                     "output_tokens\ttool_calls\twall_ms\trubric_hits\trubric_total\n")
        for effort, arm in sorted(totals):
            entry = totals[(effort, arm)]
            handle.write("%s\t%s\t%d\t%d\t%.6f\t%d\t%d\t%d\t%d\t%d\t%d\t%d\n" % (
                effort or "default", arm, entry["runs"], entry["successful"], entry["cost"], entry["input"],
                entry["uncached"], entry["output"], entry["tools"], entry["wall"], entry["hits"], entry["rubric"]))

    panoptes_version = subprocess.run([args.binary, "--version"], check=True, capture_output=True,
                                      text=True).stdout.strip()
    metadata = [
        ("timestamp_utc", time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())),
        *provenance.items(),
        ("model", args.model),
        ("client", "benchmark-agent-llm.py/2 (chat-completions)"),
        ("mcp_compact", str(args.mcp_compact).lower()),
        ("rubric_method", RUBRIC_METHOD),
        ("endpoint", base_url),
        ("trials", str(args.trials)),
        ("reasoning_efforts", ",".join(efforts) or "default"),
        ("tasks", args.tasks),
        ("max_turns", str(args.max_turns)),
        ("input_usd_per_mtok", "%.2f" % input_rate),
        ("cached_input_usd_per_mtok", "%.2f" % cached_rate),
        ("output_usd_per_mtok", "%.2f" % output_rate),
        ("panoptes", panoptes_version),
        ("python", sys.version.split()[0]),
        ("uname", " ".join(os.uname())),
    ]
    with open(os.path.join(output, "metadata.tsv"), "w", encoding="utf-8") as handle:
        handle.write("key\tvalue\n")
        for key, value in metadata:
            handle.write("%s\t%s\n" % (key, value))

    print("agent benchmark results: %s" % output)
    print("summary: %s" % summary_path)
    if args.quick and not benchmark_failed:
        tools = {}
        for arm in ("baseline", "panoptes"):
            for f in sorted(glob.glob(os.path.join(output, "runs", "*%s*.jsonl" % arm))):
                with open(f, encoding="utf-8") as h:
                    for line in h:
                        e = json.loads(line)
                        if e.get("type") == "tool_result":
                            s = tools.setdefault((arm, e["name"]), [0, 0, 0])
                            s[0] += 1
                            s[1] += e["output_chars"]
                            s[2] = max(s[2], e["output_chars"])
        print("\nquick find-bloat comparison (%s, 1 trial, effort %s):" % (args.task, "/".join(efforts)))
        for arm in ("baseline", "panoptes"):
            if not any((arm, n) in tools for n in ("panoptes_find", "panoptes_grep", "panoptes_map", "read_file", "grep_files", "glob_files")):
                continue
            row = []
            for name in ("panoptes_find", "panoptes_grep", "panoptes_map", "read_file", "grep_files", "glob_files"):
                if (arm, name) in tools:
                    c, t, m = tools[(arm, name)]
                    row.append("%s:%d/%.1fk/%d" % (name, c, t / c / 1000, m))
            print("  %-9s %s" % (arm, "  ".join(row)))
        print_quick_results(results_path)
    if benchmark_failed:
        sys.exit(1)


if __name__ == "__main__":
    main()
