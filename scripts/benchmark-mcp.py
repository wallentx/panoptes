#!/usr/bin/env python3
"""Repeatable MCP indexing benchmarks using disposable, restored fixture stores."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import queue
import shlex
import shutil
import signal
import sqlite3
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone
from contextlib import closing

REPO = Path(__file__).resolve().parent.parent
CASES = ("empty-index", "populated-index", "changed-files", "unchanged")
FORMAT = 2


def digest(path):
    checksum = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            checksum.update(block)
    return checksum.hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def readonly(path):
    return sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True)


def graph_state(store):
    """Validate the result outside timing; ignore row IDs and machine-local roots."""
    with closing(readonly(store)) as conn:
        integrity = conn.execute("pragma quick_check").fetchone()[0]
        if integrity != "ok":
            raise RuntimeError(f"fixture store integrity: {integrity}")
        state = {}
        modern = conn.execute("select 1 from sqlite_master where type='table' and name='snapshot_manifests'").fetchone()
        checkout_query = "select snapshot_id,root from checkouts where snapshot_id is not null order by root" if modern else "select id,root from repos order by root"
        for repo_id, root in conn.execute(checkout_query):
            fingerprint = hashlib.sha256()
            counts = {}
            for table in ("files", "symbols", "edges"):
                counts[table] = conn.execute(
                    f"select count(*) from {table} where repo_id=?", (repo_id,)
                ).fetchone()[0]
            queries = (
                "select path from files where repo_id=? order by path",
                "select f.path,s.name,s.kind,s.start_line,s.end_line,s.signature,s.crux,s.summary,s.container "
                "from symbols s join files f on f.id=s.file_id where s.repo_id=? "
                "order by f.path,s.start_line,s.end_line,s.kind,s.name,s.signature,s.crux,s.summary,s.container",
                "select af.path,a.name,a.kind,a.start_line,bf.path,b.name,b.kind,b.start_line,e.kind "
                "from edges e join symbols a on a.id=e.src_symbol_id "
                "join symbols b on b.id=e.dst_symbol_id join files af on af.id=a.file_id "
                "join files bf on bf.id=b.file_id where e.repo_id=? "
                "order by af.path,a.name,a.kind,a.start_line,bf.path,b.name,b.kind,b.start_line,e.kind",
            )
            queries += (
                "select f.path,s.name,s.kind,s.start_line,s.end_line,t.term,"
                "t.name_count,t.path_count,t.signature_count,t.body_count "
                "from search_terms t join symbols s on s.id=t.symbol_id "
                "join files f on f.id=s.file_id where t.repo_id=? "
                "order by f.path,s.name,s.kind,s.start_line,s.end_line,t.term,"
                "t.name_count,t.path_count,t.signature_count,t.body_count",
            )
            for query in queries:
                for row in conn.execute(query, (repo_id,)):
                    fingerprint.update(json.dumps(row, ensure_ascii=True).encode() + b"\n")
            state[Path(root).name] = {**counts, "graph_sha256": fingerprint.hexdigest()}
    return state


def snapshot(source, destination):
    with closing(readonly(source)) as src, closing(sqlite3.connect(destination)) as dst:
        src.backup(dst)


def reset_store(store, seed=None):
    # Only called with paths inside this runner's TemporaryDirectory, after all
    # processes and SQLite connections using the working store have exited.
    for suffix in ("", "-wal", "-shm"):
        store.with_name(store.name + suffix).unlink(missing_ok=True)
    if seed is not None:
        shutil.copyfile(seed, store)
        if digest(store) != digest(seed):
            raise RuntimeError("restored store differs from its seed")


def checked_run(command, env, output, label, seconds):
    with (output / f"{label}.stdout").open("wb") as stdout, (output / f"{label}.stderr").open("wb") as stderr:
        result = subprocess.run(command, env=env, stdout=stdout, stderr=stderr, timeout=seconds)
    if result.returncode:
        raise RuntimeError(f"{label} failed ({result.returncode}); see {label}.stderr")


def stop_group(process):
    # The measurement wrapper, MCP server, and its workers share this group.
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGKILL)
        process.wait()


def measure(binary, meter, store, root, env, output, label, idle_seconds, max_seconds, query):
    metrics = output / f"{label}.rusage.tsv"
    events = queue.Queue()
    start = time.monotonic()
    deadline = start + max_seconds
    with (output / f"{label}.stderr").open("wb") as stderr, (output / f"{label}.jsonl").open("w") as log:
        process = subprocess.Popen(
            [str(meter), str(metrics), str(binary), "--store", str(store), "mcp", str(root),
             "--timeout-secs", str(idle_seconds)],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
            text=True, env=env, start_new_session=True,
        )

        def read():
            try:
                for line in process.stdout:
                    events.put((time.monotonic(), json.loads(line)))
            except Exception as error:
                events.put((time.monotonic(), {"reader_error": str(error)}))
            finally:
                events.put((time.monotonic(), None))

        reader = threading.Thread(target=read, daemon=True)
        reader.start()

        def send(request_id, method, params):
            process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": request_id,
                                            "method": method, "params": params}) + "\n")
            process.stdin.flush()

        def receive():
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"{label}: benchmark exceeded --max-seconds {max_seconds}")
            observed, event = events.get(timeout=remaining)
            if event is None:
                raise RuntimeError(f"{label}: MCP closed before its response")
            log.write(json.dumps({"elapsed_ms": (observed - start) * 1000, "message": event}) + "\n")
            log.flush()
            if "reader_error" in event:
                raise RuntimeError(event["reader_error"])
            if "error" in event or event.get("result", {}).get("isError"):
                raise RuntimeError(f"{label}: {event}")
            return observed, event

        completed = False
        try:
            send(1, "initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                                   "clientInfo": {"name": "panoptes-index-benchmark", "version": str(FORMAT)}})
            while True:
                _, event = receive()
                if event.get("id") == 1:
                    break
            process.stdin.write('{"jsonrpc":"2.0","method":"notifications/initialized"}\n')
            requested = time.monotonic()
            send(2, "tools/call", {"name": "find", "arguments": {"query": query, "limit": 5, "repo": str(root)},
                                   "_meta": {"progressToken": label}})
            previous, first, updates, max_gap = requested, None, 0, 0.0
            worker_stages = []
            while True:
                observed, event = receive()
                if event.get("method") == "notifications/progress":
                    if event["params"]["progressToken"] != label:
                        raise RuntimeError("progress token does not match this trial")
                    timing = event["params"].get("_meta", {}).get("panoptesTiming")
                    if timing is not None:
                        worker_stages.append(timing)
                    updates += 1
                    first = first or observed
                    max_gap = max(max_gap, observed - previous)
                    previous = observed
                if event.get("id") == 2:
                    max_gap = max(max_gap, observed - previous)
                    data = event["result"]["structuredContent"]
                    hits = data["repositories"][root.name]["hits"]
                    if not any(hit["name"] == query for hit in hits):
                        raise RuntimeError(f"{label}: expected symbol {query!r} missing")
                    request_ms = (observed - requested) * 1000
                    break
            process.stdin.close()
            status = process.wait(timeout=max(0.1, deadline - time.monotonic()))
            reader.join(timeout=2)
            if status:
                raise RuntimeError(f"{label}: measurement wrapper exited {status}")
            completed = True
        finally:
            # Clean up descendants even if the wrapper exited early on failure.
            if not completed:
                stop_group(process)
            if process.stdin and not process.stdin.closed:
                process.stdin.close()
            reader.join(timeout=2)
            process.stdout.close()
    wall_ms, peak_rss, status = map(int, metrics.read_text().split())
    if status:
        raise RuntimeError(f"{label}: MCP exited {status}")
    if sys.platform == "darwin":
        peak_rss //= 1024
    return {"wall_ms": wall_ms, "request_ms": round(request_ms, 3), "peak_rss_kb": peak_rss,
            "worker_stages": worker_stages, "progress_updates": updates, "first_progress_ms": round((first - requested) * 1000, 3) if first else None,
            "max_progress_gap_ms": round(max_gap * 1000, 3), "db_bytes": store.stat().st_size}


def positive(value):
    number = int(value)
    if number < 1:
        raise argparse.ArgumentTypeError("must be a positive integer")
    return number


def arguments():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=REPO / "target/release/panoptes")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--files", type=positive, default=200)
    parser.add_argument("--background-repos", type=positive, default=3)
    parser.add_argument("--background-files", type=positive, default=1000)
    parser.add_argument("--changed-files", type=positive, default=20)
    parser.add_argument("--runs", type=positive, default=3)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--jobs", type=positive, default=1)
    parser.add_argument("--idle-seconds", type=positive, default=30)
    parser.add_argument("--max-seconds", type=positive, default=600, help="per-trial/setup safety ceiling; failures are never included in medians")
    parser.add_argument("--baseline", type=Path, help="compare with a compatible completed report.json")
    args = parser.parse_args()
    if args.changed_files > args.files or args.warmups < 0 or args.jobs > 32 or args.idle_seconds > 300:
        parser.error("require changed-files <= files, warmups >= 0, jobs <= 32, idle-seconds <= 300")
    args.binary = args.binary.expanduser().resolve()
    if not args.binary.is_file() or not os.access(args.binary, os.X_OK):
        parser.error("binary missing/not executable; build with cargo build --release --locked -j 1 or pass --binary")
    return args


def main():
    args = arguments()
    output = (args.output or REPO / ".local/benchmarks" / datetime.now(timezone.utc).strftime(f"mcp-%Y%m%dT%H%M%SZ-{os.getpid()}")).resolve()
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()):
        raise RuntimeError("output directory must be empty; previous reports are never overwritten")
    config = {name: getattr(args, name) for name in ("files", "background_repos", "background_files", "changed_files", "runs", "warmups", "jobs", "idle_seconds")}
    baseline = json.loads(args.baseline.read_text()) if args.baseline else None
    if baseline and (baseline.get("format") != FORMAT or baseline.get("status") != "complete" or baseline.get("config") != config):
        raise RuntimeError("baseline is incomplete or has a different benchmark format/configuration")
    env = dict(os.environ, PANOPTES_JOBS=str(args.jobs))
    report = {"format": FORMAT, "status": "running", "config": config, "samples": [], "binary": str(args.binary),
              "binary_sha256": digest(args.binary), "runner_sha256": digest(__file__), "platform": platform.platform(),
              "machine": platform.machine(), "python": sys.version, "cpu_count": os.cpu_count(),
              "timestamp_utc": datetime.now(timezone.utc).isoformat(), "max_seconds": args.max_seconds}
    if baseline and any(baseline[key] != report[key] for key in ("platform", "machine", "cpu_count")):
        raise RuntimeError("baseline host metadata differs; compare on the same machine")
    try:
        with tempfile.TemporaryDirectory(prefix="panoptes-mcp-bench-", dir=os.environ.get("TMPDIR")) as directory:
            scratch = Path(directory)
            report["scratch_directory"] = str(scratch)
            meter, store = scratch / "rusage", scratch / "working.db"
            checked_run(shlex.split(os.environ.get("CC", "cc")) + ["-O2", "-Wall", "-Wextra", "-Werror",
                        str(REPO / "bench/rusage.c"), "-o", str(meter)], env, output, "compile-meter", args.max_seconds)
            version = subprocess.check_output([str(args.binary), "--store", str(store), "version", "--json"], env=env, timeout=args.max_seconds)
            report["version"] = json.loads(version)
            report["git_head"] = subprocess.check_output(["git", "-C", str(REPO), "rev-parse", "HEAD"], text=True).strip()
            report["git_dirty"] = bool(subprocess.check_output(["git", "-C", str(REPO), "status", "--porcelain"]))
            roots = [scratch / "target"] + [scratch / f"background-{index:02}" for index in range(args.background_repos)]
            corpus = hashlib.sha256()
            for index, root in enumerate(roots):
                count = args.files if index == 0 else args.background_files
                checked_run(["sh", str(REPO / "bench/generate-fixture.sh"), str(root), str(count)], env, output, f"generate-{root.name}", args.max_seconds)
                for path in sorted(root.glob("src/*.ts")):
                    corpus.update(f"{root.name}/{path.relative_to(root)}\0".encode() + path.read_bytes() + b"\0")
            report["fixture_sha256"] = corpus.hexdigest()
            if baseline and baseline["fixture_sha256"] != report["fixture_sha256"]:
                raise RuntimeError("baseline fixture content differs")
            target = roots[0]
            originals = {path: path.read_bytes() for path in sorted(target.glob("src/*.ts"))}
            timestamps = {path: (path.stat().st_atime_ns, path.stat().st_mtime_ns) for path in originals}
            altered = list(originals)[:args.changed_files]
            seed = scratch / "seed.db"
            for root in roots[1:]:
                checked_run([str(args.binary), "--store", str(seed), "build", str(root)], env, output, f"seed-{root.name}", args.max_seconds)
            background = scratch / "background.db"
            snapshot(seed, background)
            background_state = graph_state(background)
            checked_run([str(args.binary), "--store", str(seed), "build", str(target)], env, output, "seed-target", args.max_seconds)
            indexed = scratch / "indexed.db"
            snapshot(seed, indexed)
            initial_state = graph_state(indexed)
            report["seeds"] = {"background": {"sha256": digest(background), "bytes": background.stat().st_size, "state": background_state},
                               "indexed": {"sha256": digest(indexed), "bytes": indexed.stat().st_size, "state": initial_state}}
            expected = {}
            for trial in range(args.warmups + args.runs):
                warmup = trial < args.warmups
                # Rotate scenarios so one workload is not always last/hottest.
                order = CASES[trial % len(CASES):] + CASES[:trial % len(CASES)]
                for case in order:
                    label = f"{case}-{'warmup' if warmup else 'run'}-{trial + 1:02}"
                    for path, source in originals.items():
                        path.write_bytes(source)
                        os.utime(path, ns=timestamps[path])
                    source_seed = None if case == "empty-index" else background if case == "populated-index" else indexed
                    reset_store(store, source_seed)
                    query = "process_payment_00000_0"
                    if case == "changed-files":
                        for index, path in enumerate(altered):
                            path.write_bytes(originals[path] + f"\nexport function benchmark_changed_{index:05d}() {{ return process_payment_{index:05d}_0(1); }}\n".encode())
                            os.utime(path, ns=(timestamps[path][0], timestamps[path][1] + 1_000_000_000))
                        query = "benchmark_changed_00000"
                    try:
                        sample = measure(args.binary, meter, store, target, env, output, label, args.idle_seconds, args.max_seconds, query)
                        checked_run([str(args.binary), "--store", str(store), "--no-refresh", "status", str(target), "--json"],
                                    env, output, label + "-freshness", args.max_seconds)
                        freshness = json.loads((output / f"{label}-freshness.stdout").read_text())[0]["freshness"]
                        if not freshness["indexed"] or not freshness["extractor_current"] or any(freshness[key] for key in ("added", "modified", "deleted")):
                            raise RuntimeError(f"{label}: indexing left stale source: {freshness}")
                        state = graph_state(store)
                        if state["target"]["files"] != args.files:
                            raise RuntimeError("target file count differs from generated fixture")
                        if case != "empty-index" and any(state.get(name) != data for name, data in background_state.items()):
                            raise RuntimeError("indexing the target changed a background repository")
                        if case != "changed-files" and state["target"] != initial_state["target"]:
                            raise RuntimeError("cold/warm query did not reproduce the seeded target graph")
                        if case in expected and state != expected[case]:
                            raise RuntimeError(f"{case}: repeated trial produced a different graph")
                        expected[case] = state
                        report["samples"].append({"case": case, "trial": trial + 1, "warmup": warmup, "seed_sha256": digest(source_seed) if source_seed else None, **sample})
                        print(f"{label}: {sample['request_ms']:.1f} ms, {sample['progress_updates']} updates, max gap {sample['max_progress_gap_ms']:.1f} ms", flush=True)
                    finally:
                        reset_store(store)
            report["graphs"] = expected
            if baseline and baseline["graphs"] != expected:
                raise RuntimeError("candidate graph differs from baseline; do not treat this as an equivalent-work speedup")
            report["summary"] = {}
            for case in CASES:
                samples = [sample for sample in report["samples"] if sample["case"] == case and not sample["warmup"]]
                summary = {"samples": len(samples)}
                for field in ("request_ms", "wall_ms", "peak_rss_kb", "db_bytes", "progress_updates", "max_progress_gap_ms"):
                    values = [sample[field] for sample in samples]
                    summary[field] = {"median": statistics.median(values), "min": min(values), "max": max(values)}
                if baseline:
                    before = baseline["summary"][case]["request_ms"]["median"]
                    summary["baseline_request_ms"] = before
                    summary["request_change_percent"] = round(100 * (summary["request_ms"]["median"] / before - 1), 2)
                report["summary"][case] = summary
            report["status"] = "complete"
    except BaseException as error:
        report["status"] = "failed"
        report["error"] = str(error)
        raise
    finally:
        report["scratch_removed"] = not Path(report.get("scratch_directory", "/nonexistent")).exists()
        write_json(output / "report.json", report)
        command = ["python3", str(Path(__file__).resolve()), "--binary", str(args.binary)]
        for key, value in config.items():
            command += ["--" + key.replace("_", "-"), str(value)]
        command += ["--max-seconds", str(args.max_seconds), "--output", str(output.with_name(output.name + "-rerun"))]
        (output / "README.md").write_text("# MCP indexing benchmark\n\nRerun on the same host:\n\n```sh\n" + shlex.join(command) + "\n```\n\nCompare a candidate by adding `--baseline " + str(output / "report.json") + "`.\n\nVerify artifacts with `sha256sum -c SHA256SUMS` from this directory.\n")
        (output / "SHA256SUMS").write_text("".join(f"{digest(path)}  {path.name}\n" for path in sorted(output.iterdir()) if path.is_file() and path.name != "SHA256SUMS"))
    print(f"report: {output / 'report.json'}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (RuntimeError, ValueError, OSError, subprocess.SubprocessError, queue.Empty) as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        sys.exit(1)
