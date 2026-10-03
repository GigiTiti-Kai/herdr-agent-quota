"""The bridge plugin against the real Hermes checkout. Runs only inside real_hermes_sandbox.sh.

Real here: the launcher's interpreter and prelude, Hermes' bootstrap, plugin discovery and
context, hook dispatch, the agent object, the in-session model switch, the usage parser, the
session finalizer, one whole `hermes chat -q` process (its own CLI object, agent, and hooks), and
the TUI's gateway process driven over its stdio JSON-RPC (session table, agent build, turn).
Stubbed: the HTTP answer of the usage endpoint, the notify executable, and in tests 2-6 the CLI
object that holds the agent. The sandbox has no network, so a request that escaped the stub
fails instead of reaching a provider.

Outside the sandbox this file refuses to start. Hermes' bootstrap rewrites its launchers and
syncs dependencies when it runs somewhere it can write, which is what this file must never do.
"""

import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import time
import types
import unittest
from pathlib import Path

HOME = Path(os.environ.get("HOME", "/nonexistent")) / ".hermes"
ROOT = HOME / "hermes-agent"
WORK = Path("/work")
STATE = WORK / "state"
MAILBOX = STATE / "hermes-bridge"
NOTIFY_LOG = WORK / "notify.log"
PLUGIN = "herdr-agent-quota"
SESSION = "sandbox-session-1"
KEY_A = "fake-token-" + "A" * 40
KEY_B = "fake-token-" + "B" * 40
KEY_CLI = "fake-token-" + "D" * 40
CODEX_BASE = "https://chatgpt.com/backend-api/codex"
HOOKS = (
    "on_session_start", "on_session_reset", "pre_llm_call", "pre_api_request",
    "post_api_request", "on_session_finalize",
)
SURFACE_LOG = WORK / "surface.jsonl"
PROBE = "surface-probe"
# A second plugin, only in this test home: from inside a real Hermes surface it records what
# the bridge's own adapter finds at each hook. It writes no key and no message.
PROBE_SOURCE = f'''
import json, os, sys

def register(ctx):
    def record(hook, **kwargs):
        from hermes_cli.plugins import get_plugin_manager
        bridge = next(m for m in list(sys.modules.values()) if getattr(m, "MAILBOX_DIR", None) == "hermes-bridge")
        session_id = kwargs.get("session_id")
        cli = get_plugin_manager()._cli_ref
        agent = bridge.find_live_agent(session_id) if isinstance(session_id, str) else None
        server = sys.modules.get("tui_gateway.server")
        tui_agents = [r.get("agent") for r in list(getattr(server, "_sessions", {{}}).values())]
        with open({str(SURFACE_LOG)!r}, "a") as out:
            out.write(json.dumps({{
                "hook": hook, "platform": kwargs.get("platform"), "cli": type(cli).__name__,
                "found": agent is not None, "is_cli_agent": agent is not None and agent is getattr(cli, "agent", None),
                "is_tui_agent": agent is not None and any(agent is a for a in tui_agents),
                "provider": getattr(agent, "provider", None),
                "mailbox": sorted(os.listdir({str(MAILBOX)!r})),
            }}) + "\\n")
    for hook in ("on_session_start", "pre_llm_call", "pre_api_request"):
        ctx.register_hook(hook, lambda _hook=hook, **kwargs: record(_hook, **kwargs))
'''


def refuse_outside_sandbox():
    """Exit 3 unless every boundary the sandbox promises is observable from here."""
    problems = []
    for path in (ROOT, HOME / "installs", HOME / "tools"):
        try:
            if not os.statvfs(path).f_flag & os.ST_RDONLY:
                problems.append(f"{path} is writable")
        except OSError:
            problems.append(f"{path} is missing")
    for name in ("auth.json", ".env"):
        if (HOME / name).exists():
            problems.append(f"{HOME / name} is present")
    if not (Path("/plugin-src") / "__init__.py").is_file() or not WORK.is_dir():
        problems.append("sandbox mounts /plugin-src and /work are missing")
    if not problems:  # never opened on a host that already failed the checks above
        try:
            socket.create_connection(("1.1.1.1", 53), timeout=2).close()
            problems.append("the network is reachable")
        except OSError:
            pass
    if problems:
        print("REFUSED: not inside real_hermes_sandbox.sh: " + "; ".join(problems), file=sys.stderr)
        sys.exit(3)


def reexec_with_launcher_python():
    """Hermes runs on its own pinned interpreter; take it from the launcher, as the launcher does."""
    if os.environ.get("HERDR_AGENT_QUOTA_REAL_HERMES") == "1":
        return
    launcher = (ROOT / ".hermes" / "bin" / "hermes").read_text()
    python = re.search(r"^exec (\S+) -I -c ", launcher, re.M).group(1)
    os.environ["HERDR_AGENT_QUOTA_REAL_HERMES"] = "1"
    os.execv(python, [python, "-I", __file__, *sys.argv[1:]])


def install_fixture_home():
    """What `configure` would leave in a default profile, pointing at stubs under /work."""
    plugin = HOME / "plugins" / PLUGIN
    plugin.mkdir(parents=True)
    for name in ("__init__.py", "plugin.yaml"):
        shutil.copyfile(Path("/plugin-src") / name, plugin / name)
    STATE.mkdir(mode=0o700)
    stub = WORK / "notify-stub"
    stub.write_text(f'#!/bin/sh\necho "$1 $HERDR_PLUGIN_STATE_DIR" >> {NOTIFY_LOG}\n')
    stub.chmod(0o700)
    bridge = plugin / "bridge.json"
    bridge.write_text(json.dumps({"state_dir": str(STATE), "executable": str(stub)}))
    bridge.chmod(0o600)
    probe = HOME / "plugins" / PROBE
    probe.mkdir()
    (probe / "plugin.yaml").write_text(f'name: {PROBE}\nversion: "1.0"\ndescription: test probe\n')
    (probe / "__init__.py").write_text(PROBE_SOURCE)
    write_config(PLUGIN)
    os.environ["HERDR_ENV"] = "1"


def write_config(*plugins, model=None, provider=None):
    """The allow-list as `hermes plugins enable` leaves it. The command itself cannot run here: it
    submits the selection to Hermes's package manager, which needs its install state writable.
    Retries are cut so a turn without a network ends in seconds, not minutes."""
    enabled = "".join(f"  - {name}\n" for name in plugins)
    chosen = f"model:\n  default: {model}\n  provider: {provider}\n" if model else ""
    (HOME / "config.yaml").write_text(
        f"plugins:\n  enabled:\n{enabled}agent:\n  api_max_retries: 1\n  auto_recovery_cycles: 0\n{chosen}"
    )
    os.environ["HERDR_PANE_ID"] = "w1:p1"


def hermes_cli(*arguments, timeout=120, **environment):
    """The real launcher, as a user runs it. A run that outlives `timeout` gets Ctrl+C, as a user
    would send, so Hermes still ends the session its own way. Returns `(exit code, output)`."""
    child = subprocess.Popen(
        [str(ROOT / ".hermes" / "bin" / "hermes"), *arguments], env={**os.environ, **environment},
        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
    )
    try:
        output, _ = child.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        child.send_signal(signal.SIGINT)
        try:
            output, _ = child.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            child.kill()
            output, _ = child.communicate()
            output += "\n[killed]"
        output += "\n[interrupted]"
    return child.returncode, output



def launcher_prelude():
    """The launcher's own lines, up to and including the bootstrap import."""
    os.environ.pop("PYTHONHOME", None)
    os.environ.pop("PYTHONPATH", None)
    sys.path.insert(0, str(ROOT))
    from hermes_constants import get_default_hermes_root

    os.environ["HERMES_HOME"] = os.environ.get("HERMES_HOME") or str(get_default_hermes_root())
    import hermes_bootstrap  # noqa: F401 - dependency activation is its import side effect


def record():
    try:
        return json.loads((MAILBOX / f"{SESSION}.json").read_text())
    except (OSError, ValueError):
        return None


def wait_for(predicate, seconds=15.0):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    return None


class RealHermes(unittest.TestCase):
    """One session's life, in order. Each step builds on the one before."""

    state = types.SimpleNamespace(observed=[], requests=[])

    def test_1_real_discovery_loads_the_plugin_and_its_hooks(self):
        from hermes_cli import plugins

        plugins.discover_plugins()
        manager = plugins.get_plugin_manager()
        loaded = manager._plugins[PLUGIN]
        self.assertTrue(loaded.enabled)
        self.assertIsNone(loaded.error)
        for hook in HOOKS:
            self.assertIn(hook, plugins.VALID_HOOKS)
            self.assertEqual(len(manager._hooks[hook]), 1, hook)
        modules = [m for m in list(sys.modules.values()) if getattr(m, "MAILBOX_DIR", None) == "hermes-bridge"]
        self.assertEqual(len(modules), 1)
        self.state.plugin = modules[0]
        self.assertTrue(MAILBOX.is_dir(), "register() built no bridge: a precondition failed")
        self.assertEqual(MAILBOX.stat().st_mode & 0o777, 0o700)

        observe = self.state.plugin.Bridge.observe
        seen = self.state.observed

        def counted(bridge, session_id, activity=False):
            seen.append((session_id, activity))
            return observe(bridge, session_id, activity)

        self.state.plugin.Bridge.observe = counted

    def test_2_a_real_agent_reads_as_a_supported_codex_route(self):
        from hermes_cli import plugins
        from run_agent import AIAgent

        agent = AIAgent(
            base_url=CODEX_BASE, api_key=KEY_A, provider="openai-codex", api_mode="codex_responses",
            model="gpt-5.5", quiet_mode=True, platform="cli", session_id=SESSION,
            skip_context_files=True, skip_memory=True, skip_background_review=True, enabled_toolsets=[],
        )
        self.state.agent = agent
        signature, model, secret = self.state.plugin.read_route(agent, b"k" * 32)
        self.assertEqual((signature.provider, signature.supported), ("openai-codex", True))
        self.assertEqual(model, "gpt-5.5")
        self.assertEqual(secret, (CODEX_BASE, KEY_A))
        # Hermes' interactive CLI and its single-query mode both do exactly this assignment.
        for source in ("hermes_cli/cli_tui_mixin.py", "hermes_cli/cli_single_query.py"):
            self.assertIn("get_plugin_manager()._cli_ref = ", (ROOT / source).read_text())
        self.assertIn("_sessions: dict[str, dict] = {}", (ROOT / "tui_gateway/server.py").read_text())
        plugins.get_plugin_manager()._cli_ref = types.SimpleNamespace(agent=agent)
        self.assertIs(self.state.plugin.find_live_agent(SESSION), agent)

    def test_3_real_hook_dispatch_reaches_the_bridge_and_writes_the_route(self):
        from agent.turn_api_request import _fire_pre_api_request_hook
        from agent.turn_context import _collect_pre_llm_call_context
        from hermes_cli.lifecycle import invoke_hook

        agent, seen = self.state.agent, self.state.observed
        # The keyword set of agent/conversation_loop.py's own on_session_start call.
        invoke_hook("on_session_start", session_id=agent.session_id, model=agent.model, platform=agent.platform or "")
        self.assertEqual(seen, [(SESSION, False)])
        first = record()
        self.assertEqual(first["route"]["provider"], "openai-codex")
        self.assertTrue(first["route"]["supported"])
        self.assertEqual(first["route"]["epoch"], 1)
        self.assertIsNone(first["quota"])
        self.assertEqual((first["pane_id"], first["profile"], first["session_id"]), ("w1:p1", "default", SESSION))
        self.assertEqual((MAILBOX / f"{SESSION}.json").stat().st_mode & 0o777, 0o600)

        _collect_pre_llm_call_context(
            agent, effective_task_id="task", turn_id="turn", original_user_message="hi",
            messages=[], conversation_history=None,
        )
        self.assertEqual(seen[-1], (SESSION, False))
        self.assertEqual(len(seen), 2)
        _fire_pre_api_request_hook(
            agent, {"input": []}, [], [], messages=[], original_user_message="hi", approx_tokens=0,
            total_chars=0, retry_count=0, api_call_count=1, api_request_id="request", api_start_time=time.time(),
            effective_task_id="task", turn_id="turn",
        )
        self.assertEqual(len(seen), 3)
        self.assertEqual(record()["route"]["epoch"], 1, "an unchanged route must not start a new epoch")

    def test_4_the_real_usage_parser_fills_the_quota_for_that_key_only(self):
        import httpx
        from agent import account_usage
        from agent.turn_response_intake import _fire_post_api_request_hook

        agent, requests = self.state.agent, self.state.requests
        reset = int(time.time()) + 3600

        def answer(request):
            requests.append((str(request.url), request.headers.get("authorization") == f"Bearer {KEY_A}"))
            return httpx.Response(200, json={"plan_type": "plus", "rate_limit": {
                "primary_window": {"used_percent": 12, "limit_window_seconds": 18000, "reset_at": reset},
                "secondary_window": {"used_percent": 34, "limit_window_seconds": 604800, "reset_at": reset},
            }})

        account_usage.httpx = types.SimpleNamespace(
            Client=lambda **kwargs: httpx.Client(transport=httpx.MockTransport(answer), **kwargs),
            HTTPStatusError=httpx.HTTPStatusError,
        )
        _fire_post_api_request_hook(
            agent, types.SimpleNamespace(model="gpt-5.5", usage=None),
            types.SimpleNamespace(role="assistant", content="ok", tool_calls=None), "stop", api_messages=[],
            api_call_count=1, api_duration=0.1, api_start_time=time.time(), api_request_id="request",
            effective_task_id="task", turn_id="turn",
        )
        self.assertEqual(self.state.observed[-1], (SESSION, True))
        filled = wait_for(lambda: (record() or {}).get("quota") and record())
        self.assertIsNotNone(filled, "no quota reached the mailbox")
        self.assertEqual(requests, [("https://chatgpt.com/backend-api/wham/usage", True)])
        self.assertEqual(filled["quota"]["epoch"], filled["route"]["epoch"])
        self.assertEqual(filled["quota"]["identity"], filled["route"]["identity"])
        self.assertEqual(filled["quota"]["windows"], [
            {"kind": "5h", "used_percent": 12.0, "resets_at": reset},
            {"kind": "7d", "used_percent": 34.0, "resets_at": reset},
        ])
        self.state.first_identity = filled["route"]["identity"]

    def test_5_a_real_idle_switch_drops_the_quota_without_any_hook(self):
        from agent.agent_runtime_helpers import switch_model

        agent, hooks_before = self.state.agent, len(self.state.observed)
        switch_model(agent, "gpt-5.5", "openai-codex", api_key=KEY_B, base_url=CODEX_BASE, api_mode="codex_responses")
        moved = wait_for(lambda: (record() or {}).get("route", {}).get("epoch") == 2 and record())
        self.assertIsNotNone(moved, "the poller did not follow a key change on an idle session")
        self.assertIsNone(moved["quota"])
        self.assertTrue(moved["route"]["supported"])
        self.assertNotEqual(moved["route"]["identity"], self.state.first_identity)

        # Not Anthropic: its SDK is an optional extra here, and Hermes answers a switch to it by
        # starting a dependency install (which the read-only mounts refuse).
        switch_model(agent, "anthropic/claude-sonnet-4.5", "openrouter", api_key="fake-token-" + "C" * 40,
                     base_url="https://openrouter.ai/api/v1", api_mode="chat_completions")
        other = wait_for(lambda: (record() or {}).get("route", {}).get("epoch") == 3 and record())
        self.assertIsNotNone(other, "the poller did not follow a provider change on an idle session")
        self.assertEqual((other["route"]["provider"], other["route"]["supported"]), ("openrouter", False))
        self.assertIsNone(other["route"]["identity"])
        self.assertIsNone(other["quota"])
        self.assertEqual(len(self.state.observed), hooks_before, "no hook fired; the poller did this")
        self.assertEqual(len(self.state.requests), 1, "nothing was asked for the new routes")

    def test_6_the_real_finalizer_releases_the_mailbox(self):
        from hermes_cli.lifecycle import finalize_session

        finalize_session(session_id=SESSION, platform="cli")
        self.assertEqual(sorted(p.name for p in MAILBOX.iterdir()), [])

    def test_7_a_real_cli_session_is_found_through_the_real_cli_reference(self):
        # A whole `hermes chat -q` process: Hermes builds the CLI object and the agent, and fires
        # its hooks itself. The model request then dies on the missing network.
        write_config(PLUGIN, PROBE)
        _code, output = hermes_cli(
            "chat", "-Q", "-q", "hi", "--provider", "openrouter", "-m", "openai/gpt-4o-mini",
            OPENROUTER_API_KEY=KEY_CLI, HERDR_PANE_ID="w1:p2",
        )
        print(f"\n--- hermes chat -q (tail) ---\n{output[-1500:]}\n---", file=sys.stderr)
        self.assertTrue(SURFACE_LOG.is_file(), output[-2000:])
        seen = [json.loads(line) for line in SURFACE_LOG.read_text().splitlines()]
        self.assertEqual({entry["hook"] for entry in seen}, {"on_session_start", "pre_llm_call", "pre_api_request"})
        for entry in seen:
            self.assertEqual(entry["cli"], "HermesCLI", entry)
            self.assertEqual(entry["platform"], "cli", entry)
            self.assertTrue(entry["found"] and entry["is_cli_agent"], entry)
            self.assertEqual(entry["provider"], "openrouter", entry)
        # By the first request the bridge held this session's lock and had written its route.
        last = seen[-1]["mailbox"]
        self.assertEqual(len(last), 2, last)
        self.assertEqual({name.rpartition(".")[2] for name in last}, {"json", "lock"})
        # ... and released both when the process ended.
        self.assertEqual(sorted(p.name for p in MAILBOX.iterdir()), [])

    def test_8_a_real_tui_gateway_session_is_found_through_its_session_table(self):
        # The process the TUI's Node front end spawns (`python -m tui_gateway.entry`), started the
        # way the launcher starts any module, and driven over its own stdio JSON-RPC. Only the
        # Node renderer is missing; session creation, the agent build, and the turn are Hermes's.
        SURFACE_LOG.unlink(missing_ok=True)
        write_config(PLUGIN, PROBE, model="openai/gpt-4o-mini", provider="openrouter")
        child = subprocess.Popen(
            [str(ROOT / ".hermes" / "bin" / "hermes"), "--run-module", "tui_gateway.entry"],
            env={**os.environ, "OPENROUTER_API_KEY": KEY_CLI, "HERDR_PANE_ID": "w1:p3"},
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, bufsize=1,
        )
        lines = []

        def read_until(predicate, seconds=120):
            deadline = time.monotonic() + seconds
            while time.monotonic() < deadline:
                line = child.stdout.readline()
                if not line:
                    return None
                try:
                    message = json.loads(line)
                except ValueError:
                    continue
                lines.append(message)
                if predicate(message):
                    return message
            return None

        def call(rid, method, params):
            child.stdin.write(json.dumps({"jsonrpc": "2.0", "id": rid, "method": method, "params": params}) + "\n")
            child.stdin.flush()
            return read_until(lambda m: m.get("id") == rid)

        try:
            ready = read_until(lambda m: (m.get("params") or {}).get("type") == "gateway.ready")
            self.assertIsNotNone(ready, lines[-5:])
            created = call(1, "session.create", {"cols": 80})
            self.assertIn("result", created, created)
            submitted = call(2, "prompt.submit", {"session_id": created["result"]["session_id"], "text": "hi"})
            self.assertIn("result", submitted, submitted)
            seen = wait_for(lambda: SURFACE_LOG.is_file() and [
                json.loads(line) for line in SURFACE_LOG.read_text().splitlines()
            ] if SURFACE_LOG.is_file() and "pre_api_request" in SURFACE_LOG.read_text() else None, 120)
            self.assertTrue(seen, lines[-5:])
            hooks = {entry["hook"] for entry in seen}
            self.assertIn("pre_llm_call", hooks)
            self.assertIn("pre_api_request", hooks)
            for entry in seen:
                self.assertEqual(entry["platform"], "tui", entry)
                self.assertTrue(entry["found"] and entry["is_tui_agent"], entry)
                self.assertEqual(entry["provider"], "openrouter", entry)
            last = seen[-1]["mailbox"]
            self.assertEqual({name.rpartition(".")[2] for name in last}, {"json", "lock"}, last)
            stored = created["result"]["stored_session_id"]
            self.assertEqual(sorted(last), [f"{stored}.json", f"{stored}.lock"])
            route = json.loads((MAILBOX / f"{stored}.json").read_text())["route"]
            self.assertEqual((route["provider"], route["supported"], route["identity"]), ("openrouter", False, None))
            self.assertEqual(json.loads((MAILBOX / f"{stored}.json").read_text())["pane_id"], "w1:p3")
        finally:
            child.stdin.close()
            try:
                child.wait(60)
            except subprocess.TimeoutExpired:
                child.send_signal(signal.SIGINT)
                try:
                    child.wait(30)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
            child.stdout.close()
        self.assertEqual(sorted(p.name for p in MAILBOX.iterdir()), [], "the gateway left its mailbox behind")

    def test_9_only_the_notify_stub_ran_and_no_key_was_written(self):
        lines = wait_for(lambda: NOTIFY_LOG.is_file() and NOTIFY_LOG.read_text().splitlines())
        self.assertTrue(lines)
        self.assertEqual(set(lines), {f"hermes-notify {STATE}"})
        for path in WORK.rglob("*"):
            if path.is_file():
                data = path.read_bytes()
                for key in (KEY_A, KEY_B, KEY_CLI):
                    self.assertNotIn(key.encode(), data, path)


if __name__ == "__main__":
    refuse_outside_sandbox()
    reexec_with_launcher_python()
    install_fixture_home()
    launcher_prelude()
    unittest.main(verbosity=2)
