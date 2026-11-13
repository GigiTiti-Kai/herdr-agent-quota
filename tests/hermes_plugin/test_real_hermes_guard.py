"""test_real_hermes.py must do nothing when loaded outside real_hermes_sandbox.sh.

It once ran under `unittest discover` and overwrote the real ~/.hermes/config.yaml. Every case
here runs a child with HOME pointed at a new temporary directory, never the caller's HOME.
"""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent

DISCOVER = """
import sys, unittest
suite = unittest.defaultTestLoader.discover(sys.argv[1], pattern="test_real_hermes.py")
result = unittest.TextTestRunner(stream=sys.stderr).run(suite)
print(result.testsRun, len(result.skipped), len(result.errors) + len(result.failures))
"""

DIRECT_WRITES = """
import sys
sys.path.insert(0, sys.argv[1])
import test_real_hermes as t
for call in (lambda: t.write_config("x"), t.install_fixture_home):
    try:
        call()
    except RuntimeError:
        print("refused")
    else:
        print("WROTE")
"""

DIRECT_RUN = """
import sys, unittest
sys.path.insert(0, sys.argv[1])
import test_real_hermes as t
result = unittest.TestResult()
t.RealHermes("test_1_real_discovery_loads_the_plugin_and_its_hooks").run(result)
print(result.testsRun, len(result.skipped), len(result.errors) + len(result.failures))
"""

CLAUDE_CREDENTIALS = """
import sys
sys.path.insert(0, sys.argv[1])
import test_real_hermes as t
try:
    t.write_claude_credentials("x")
except RuntimeError:
    print("refused")
else:
    print("WROTE")
"""

PROBLEMS = """
import sys
sys.path.insert(0, sys.argv[1])
import test_real_hermes as t
for problem in t.sandbox_problems(probe_network=False):
    print(problem)
"""
TOKEN = "f" * 32
NOT_FRESH = "is not the fresh Hermes home real_hermes_sandbox.sh made for this run"


def run_child(script, home, **environment):
    return subprocess.run(
        [sys.executable, "-B", "-I", "-c", script, str(HERE)],
        env={"HOME": home, "PATH": "/usr/bin:/bin", **environment},
        capture_output=True, text=True, timeout=60,
    )


class RealHermesGuardTests(unittest.TestCase):
    def test_discover_outside_the_sandbox_runs_no_test_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as home:
            child = run_child(DISCOVER, home)
            self.assertEqual(child.returncode, 0, child.stderr)
            self.assertEqual(child.stdout.split(), ["0", "1", "0"], child.stderr)
            self.assertEqual(list(Path(home).rglob("*")), [])

    def test_the_write_helpers_refuse_outside_the_sandbox(self):
        with tempfile.TemporaryDirectory() as home:
            child = run_child(DIRECT_WRITES, home)
            self.assertEqual(child.stdout.split(), ["refused", "refused"], child.stderr)
            self.assertEqual(list(Path(home).rglob("*")), [])

    def test_a_test_run_directly_skips_and_writes_nothing(self):
        with tempfile.TemporaryDirectory() as home:
            child = run_child(DIRECT_RUN, home)
            self.assertEqual(child.stdout.split(), ["1", "1", "0"], child.stderr)
            self.assertEqual(list(Path(home).rglob("*")), [])

    def test_the_claude_credentials_helper_refuses_outside_the_sandbox(self):
        with tempfile.TemporaryDirectory() as home:
            child = run_child(CLAUDE_CREDENTIALS, home)
            self.assertEqual(child.stdout.split(), ["refused"], child.stderr)

    def test_a_home_without_this_runs_token_is_not_the_sandbox_home(self):
        # The real ~/.hermes has no marker; a stale or copied marker does not match a new token.
        with tempfile.TemporaryDirectory() as home:
            hermes = Path(home) / ".hermes"
            hermes.mkdir()
            self.assertIn(NOT_FRESH, run_child(PROBLEMS, home, HERDR_AGENT_QUOTA_SANDBOX_HOME_TOKEN=TOKEN).stdout)
            (hermes / ".real-hermes-sandbox-home").write_text(TOKEN)
            self.assertIn(NOT_FRESH, run_child(PROBLEMS, home).stdout)
            self.assertIn(NOT_FRESH, run_child(PROBLEMS, home, HERDR_AGENT_QUOTA_SANDBOX_HOME_TOKEN="e" * 32).stdout)
            self.assertNotIn(NOT_FRESH, run_child(PROBLEMS, home, HERDR_AGENT_QUOTA_SANDBOX_HOME_TOKEN=TOKEN).stdout)

    def test_a_hermes_home_elsewhere_is_refused(self):
        with tempfile.TemporaryDirectory() as home, tempfile.TemporaryDirectory() as other:
            child = run_child(PROBLEMS, home, HERMES_HOME=other)
            self.assertIn(f"HERMES_HOME points at {other}", child.stdout, child.stderr)
            child = run_child(PROBLEMS, home, HERMES_HOME=str(Path(home) / ".hermes"))
            self.assertNotIn("HERMES_HOME points at", child.stdout, child.stderr)


if __name__ == "__main__":
    unittest.main()
