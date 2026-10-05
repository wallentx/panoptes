"""End-to-end checks for the benchmark's reset, isolation, and comparison contract."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

REPO = Path(__file__).resolve().parent.parent
RUNNER = REPO / "scripts/benchmark-mcp.py"
BINARY = Path(os.environ.get("PANOPTES_BENCH_TEST_BIN", REPO / "target/debug/panoptes")).resolve()


class BenchmarkTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="panoptes-bench-test-", dir=os.environ.get("TMPDIR"))
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = dict(os.environ, XDG_DATA_HOME=str(self.root / "unused-default-store"))

    def run_benchmark(self, output, *extra):
        return subprocess.run(
            [sys.executable, "-B", str(RUNNER), "--binary", str(BINARY), "--files", "4",
             "--background-repos", "1", "--background-files", "3", "--changed-files", "2",
             "--runs", "2", "--warmups", "0", "--output", str(output), *extra],
            env=self.env, text=True, capture_output=True, timeout=90,
        )

    def test_trials_reset_and_compare_without_touching_default_store(self):
        first = self.root / "baseline"
        result = self.run_benchmark(first)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        report = json.loads((first / "report.json").read_text())
        self.assertEqual(report["status"], "complete")
        self.assertTrue(report["scratch_removed"])
        self.assertFalse(Path(report["scratch_directory"]).exists())
        self.assertFalse((self.root / "unused-default-store").exists())
        self.assertEqual(len(report["samples"]), 8)
        self.assertEqual(report["seeds"]["background"]["state"]["background-00"]["files"], 3)
        self.assertEqual(report["graphs"]["changed-files"]["target"]["symbols"],
                         report["graphs"]["unchanged"]["target"]["symbols"] + 2)
        for case in report["summary"]:
            samples = [sample for sample in report["samples"] if sample["case"] == case]
            self.assertEqual(samples[0]["seed_sha256"], samples[1]["seed_sha256"])
            self.assertTrue(all(sample["progress_updates"] > 0 for sample in samples))
        for line in (first / "SHA256SUMS").read_text().splitlines():
            expected, filename = line.split("  ", 1)
            self.assertEqual(hashlib.sha256((first / filename).read_bytes()).hexdigest(), expected)
        candidate = self.root / "candidate"
        compared = self.run_benchmark(candidate, "--baseline", str(first / "report.json"))
        self.assertEqual(compared.returncode, 0, compared.stdout + compared.stderr)
        second = json.loads((candidate / "report.json").read_text())
        self.assertEqual(report["fixture_sha256"], second["fixture_sha256"])
        self.assertEqual(report["graphs"], second["graphs"])
        self.assertTrue(all("request_change_percent" in data for data in second["summary"].values()))
        bad = self.run_benchmark(self.root / "different", "--files", "5", "--baseline", str(first / "report.json"))
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("different benchmark", bad.stderr)
        preserved = (first / "report.json").read_bytes()
        overwrite = self.run_benchmark(first)
        self.assertNotEqual(overwrite.returncode, 0)
        self.assertEqual((first / "report.json").read_bytes(), preserved)

    def test_mcp_failure_is_retained_and_fixture_is_removed(self):
        # Let setup use the real binary, but make the measured MCP fail. The
        # wrapper itself never receives the user's default store as an argument.
        wrapper = self.root / "fail-mcp"
        import shlex
        wrapper.write_text("#!/bin/sh\nfor arg do\n  if [ \"$arg\" = mcp ]; then exit 9; fi\ndone\nexec " + shlex.quote(str(BINARY)) + " \"$@\"\n")
        wrapper.chmod(0o700)
        output = self.root / "failed"
        result = self.run_benchmark(output, "--binary", str(wrapper))
        self.assertNotEqual(result.returncode, 0)
        report = json.loads((output / "report.json").read_text())
        self.assertEqual(report["status"], "failed")
        self.assertTrue(report["scratch_removed"])
        self.assertNotIn("summary", report)
        self.assertFalse((self.root / "unused-default-store").exists())


if __name__ == "__main__":
    if not BINARY.is_file():
        raise SystemExit("build panoptes first, or set PANOPTES_BENCH_TEST_BIN")
    unittest.main()
