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


def run_child(script, home):
    return subprocess.run(
        [sys.executable, "-B", "-I", "-c", script, str(HERE)],
        env={"HOME": home, "PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=60,
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


if __name__ == "__main__":
    unittest.main()
