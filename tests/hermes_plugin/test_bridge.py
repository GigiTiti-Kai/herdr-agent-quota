"""Unit tests for the Hermes bridge plugin. Standard library only; Hermes is not imported.

Every credential here is a made-up string, and every quota number is a fixture. Each test ends
by scanning the mailbox directory for those made-up credentials.

Run: python3 tests/hermes_plugin/test_bridge.py
"""

import fcntl
import importlib.util
import json
import os
import stat
import sys
import tempfile
import threading
import time
import types
import unittest
from pathlib import Path

PLUGIN = Path(__file__).resolve().parents[2] / "assets/hermes/herdr-agent-quota/__init__.py"
spec = importlib.util.spec_from_file_location("herdr_agent_quota_bridge", PLUGIN)
bridge_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bridge_module)

KEY_A = "fake-token-AAAAAAAAAAAAAAAAAAAAAAAA"
KEY_B = "fake-token-BBBBBBBBBBBBBBBBBBBBBBBB"
SECRETS = (KEY_A, KEY_B)
CODEX_BASE = "https://chatgpt.com/backend-api/codex"
HMAC_KEY = b"k" * 32
WEEK = [{"kind": "7d", "used_percent": 12.0, "resets_at": 4_000_000_000}]


class FakeAgent:
    def __init__(self, session_id="s1", key=KEY_A, provider="openai-codex",
                 api_mode="codex_responses", base_url=CODEX_BASE, model="model-a"):
        self.session_id = session_id
        self.provider = provider
        self.api_mode = api_mode
        self.base_url = base_url
        self.api_key = key
        self.model = model
        self._credential_pool_entry_id = None
        self._client_kwargs = {"api_key": key, "base_url": base_url}

    def set_key(self, key, entry_id=None):
        self.api_key = key
        self._client_kwargs["api_key"] = key
        self._credential_pool_entry_id = entry_id


class Inline:
    """A 'thread' that runs its target when started. The poller is never run this way."""

    def __init__(self, target, args):
        self._target, self._args = target, args

    def start(self):
        if self._target.__name__ != "_poll_loop":
            self._target(*self._args)

    def is_alive(self):
        return self._target.__name__ == "_poll_loop"


class BridgeCase(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self._tmp.name) / "hermes-bridge"
        self.dir.mkdir(mode=0o700)
        self.agents = {"s1": FakeAgent()}
        self.clock = [1_000_000.0]
        self.fetched = []
        self.answer = WEEK
        self.notified = []
        self.bridge = self.make_bridge()

    def tearDown(self):
        self.bridge.close()
        self.assert_no_secret_on_disk()
        self._tmp.cleanup()

    def make_bridge(self, **overrides):
        options = dict(
            find_agent=lambda session_id: self.agents.get(session_id),
            fetch_windows=self.fetch,
            notify=lambda: self.notified.append(self.clock[0]),
            now=lambda: self.clock[0],
            spawn_thread=lambda target, *args: Inline(target, args),
            poll_seconds=3600,
        )
        options.update(overrides)
        return bridge_module.Bridge(self.dir, "w1:p1", **options)

    def fetch(self, base_url, api_key):
        self.fetched.append((base_url, api_key))
        return self.answer

    def record(self, session_id="s1"):
        return json.loads((self.dir / (session_id + ".json")).read_text())

    def assert_no_secret_on_disk(self):
        for root, _dirs, files in os.walk(self._tmp.name):
            for name in files:
                data = Path(root, name).read_bytes()
                for secret in SECRETS:
                    self.assertNotIn(secret.encode(), data, name)

    def advance(self, seconds):
        self.clock[0] += seconds


class RouteTests(unittest.TestCase):
    def test_a_codex_route_is_supported_and_its_identity_hides_the_key(self):
        agent = FakeAgent()
        signature, model, secret = bridge_module.read_route(agent, HMAC_KEY)
        self.assertEqual((signature.provider, signature.supported, model), ("openai-codex", True, "model-a"))
        self.assertRegex(signature.identity, r"^[0-9a-f]{64}$")
        self.assertNotIn(KEY_A, signature.identity)
        self.assertEqual(secret, (CODEX_BASE, KEY_A))
        self.assertEqual(signature, bridge_module.read_route(agent, HMAC_KEY)[0])
        # Another process uses another key, so identities cannot be compared across processes.
        self.assertNotEqual(signature, bridge_module.read_route(agent, b"z" * 32)[0])

    def test_a_new_token_or_pool_entry_is_a_new_identity(self):
        agent = FakeAgent()
        first = bridge_module.read_route(agent, HMAC_KEY)[0]
        agent.set_key(KEY_B)
        rotated = bridge_module.read_route(agent, HMAC_KEY)[0]
        agent.set_key(KEY_A, entry_id="entry-2")
        same_token_other_entry = bridge_module.read_route(agent, HMAC_KEY)[0]
        self.assertEqual(len({first, rotated, same_token_other_entry}), 3)

    def test_anything_but_the_chatgpt_codex_backend_is_unsupported(self):
        cases = {
            "another provider": dict(provider="anthropic"),
            "xai oauth": dict(provider="xai-oauth"),
            "another api mode": dict(api_mode="chat_completions"),
            "custom relay host": dict(base_url="https://relay.example/backend-api/codex"),
            "look-alike host": dict(base_url="https://chatgpt.com.evil.example/backend-api/codex"),
            "plain http": dict(base_url="http://chatgpt.com/backend-api/codex"),
            "userinfo": dict(base_url="https://chatgpt.com@evil.example/backend-api/codex"),
            "another port": dict(base_url="https://chatgpt.com:8443/backend-api/codex"),
            "another path": dict(base_url="https://chatgpt.com/v1"),
            "path prefix trick": dict(base_url="https://chatgpt.com/backend-apix/codex"),
        }
        for label, overrides in cases.items():
            signature, _model, secret = bridge_module.read_route(FakeAgent(**overrides), HMAC_KEY)
            self.assertFalse(signature.supported, label)
            self.assertIsNone(signature.identity, label)
            self.assertIsNone(secret, label)

    def test_a_key_the_agent_does_not_plainly_hold_is_unsupported(self):
        rotating = FakeAgent()
        rotating.api_key = lambda: KEY_A  # Hermes installs a callable for a rotating source
        empty = FakeAgent(key="")
        client_disagrees = FakeAgent()
        client_disagrees._client_kwargs["api_key"] = KEY_B
        endpoint_disagrees = FakeAgent()
        endpoint_disagrees._client_kwargs["base_url"] = "https://relay.example/v1"
        no_client = FakeAgent()
        no_client._client_kwargs = None
        for agent in (rotating, empty, client_disagrees, endpoint_disagrees, no_client):
            signature, _model, secret = bridge_module.read_route(agent, HMAC_KEY)
            self.assertEqual((signature.supported, secret), (False, None))

    def test_a_route_caught_mid_swap_is_unknown(self):
        class Swapping(FakeAgent):
            reads = 0

            @property
            def api_key(self):
                type(self).reads += 1
                return KEY_A if type(self).reads == 1 else KEY_B

            @api_key.setter
            def api_key(self, _value):
                pass

        signature, _model, secret = bridge_module.read_route(Swapping(), HMAC_KEY)
        self.assertEqual((signature, secret), (bridge_module._UNKNOWN, None))


class MailboxTests(BridgeCase):
    def test_the_first_observation_writes_a_route_and_the_first_tick_its_quota(self):
        self.bridge.observe("s1")
        first = self.record()
        self.assertEqual(first["route"]["epoch"], 1)
        self.assertIsNone(first["quota"])
        self.assertEqual(self.fetched, [])  # a hook never fetches

        self.bridge.tick()
        record = self.record()
        self.assertEqual(self.fetched, [(CODEX_BASE, KEY_A)])
        self.assertEqual(
            set(record), {"schema", "session_id", "pane_id", "profile", "pid", "route", "quota"}
        )
        self.assertEqual(set(record["route"]), {"epoch", "provider", "supported", "identity", "observed_at"})
        self.assertEqual(set(record["quota"]), {"epoch", "identity", "fetched_at", "windows"})
        self.assertEqual((record["schema"], record["session_id"], record["pane_id"]), (1, "s1", "w1:p1"))
        self.assertEqual(record["quota"]["epoch"], record["route"]["epoch"])
        self.assertEqual(record["quota"]["identity"], record["route"]["identity"])
        self.assertEqual(record["quota"]["windows"], WEEK)
        mode = stat.S_IMODE(os.stat(self.dir / "s1.json").st_mode)
        self.assertEqual(mode, 0o600)
        self.assertEqual(len(self.notified), 1)

    def test_a_session_is_asked_about_at_most_once_a_minute(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        for _ in range(5):
            self.advance(10)
            self.bridge.observe("s1", activity=True)
            self.bridge.tick()
        self.assertEqual(len(self.fetched), 1)
        self.advance(10)  # 60s since the attempt started
        self.bridge.tick()
        self.assertEqual(len(self.fetched), 2)
        # Idle: no activity, no further request however long the poller runs.
        for _ in range(10):
            self.advance(120)
            self.bridge.tick()
        self.assertEqual(len(self.fetched), 2)

    def test_the_route_check_is_restamped_quietly_while_somebody_watches_the_agent(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        first, nudges = self.record(), len(self.notified)
        self.advance(bridge_module.HEARTBEAT_SECONDS - 1)
        self.bridge.tick()
        self.assertEqual(self.record(), first)
        self.advance(1)
        self.bridge.tick()  # the poller keeps the check fresh ...
        beat = self.record()
        self.assertEqual(beat["route"]["observed_at"], int(self.clock[0]))
        self.assertEqual(beat["quota"], first["quota"])
        self.assertEqual(beat["route"]["epoch"], first["route"]["epoch"])
        self.advance(bridge_module.HEARTBEAT_SECONDS)
        self.bridge.observe("s1")  # ... and so does a hook when the poller is gone
        self.assertEqual(self.record()["route"]["observed_at"], int(self.clock[0]))
        self.assertEqual(len(self.fetched), 1)
        self.assertEqual(len(self.notified), nudges, "a heartbeat must not repaint the sidebar")

    def test_a_check_that_resumes_after_the_reader_gave_up_nudges_the_sidebar_once(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        nudges = len(self.notified)
        self.advance(bridge_module.ROUTE_TTL_SECONDS + 1)  # a suspended process, a stuck poller
        self.bridge.tick()
        self.assertEqual(len(self.notified), nudges + 1)
        self.advance(bridge_module.HEARTBEAT_SECONDS)
        self.bridge.tick()
        self.assertEqual(len(self.notified), nudges + 1)

    def test_a_new_credential_is_asked_at_once_but_a_returning_one_keeps_its_debounce(self):
        self.answer = None  # every attempt fails, so the backoff for KEY_A grows
        self.bridge.observe("s1")
        self.bridge.tick()
        self.advance(bridge_module.DEBOUNCE_SECONDS * 2 + 1)
        self.bridge.observe("s1", activity=True)
        self.bridge.tick()
        self.assertEqual(self.fetched, [(CODEX_BASE, KEY_A)] * 2)
        self.answer = WEEK
        self.agents["s1"].set_key(KEY_B)  # the pool moved on, maybe because A ran out
        self.bridge.tick()
        self.assertEqual(self.fetched[-1], (CODEX_BASE, KEY_B))
        self.assertEqual(self.record()["quota"]["windows"], WEEK)
        self.agents["s1"].set_key(KEY_A)  # back to A inside A's backoff
        self.bridge.tick()
        self.assertEqual(len(self.fetched), 3, "A is still backing off")
        self.assertIsNone(self.record()["quota"])

    def test_a_credential_rotation_drops_the_quota_before_anything_is_fetched(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        old = self.record()
        self.advance(5)
        self.agents["s1"].set_key(KEY_B, entry_id="entry-2")
        self.bridge.observe("s1")  # what pre_api_request does on the rotated attempt
        rotated = self.record()
        self.assertEqual(rotated["route"]["epoch"], 2)
        self.assertEqual(rotated["route"]["provider"], "openai-codex")
        self.assertNotEqual(rotated["route"]["identity"], old["route"]["identity"])
        self.assertIsNone(rotated["quota"])
        # The new credential has never been asked, so the first tick asks about it.
        self.bridge.tick()
        fresh = self.record()
        self.assertEqual(len(self.fetched), 2)
        self.assertEqual(self.fetched[-1], (CODEX_BASE, KEY_B))
        self.assertEqual(fresh["quota"]["epoch"], 2)
        self.assertEqual(fresh["quota"]["identity"], fresh["route"]["identity"])

    def test_a_refreshed_token_on_the_same_pool_entry_is_also_a_new_epoch(self):
        self.agents["s1"].set_key(KEY_A, entry_id="entry-1")
        self.bridge.observe("s1")
        self.bridge.tick()
        self.agents["s1"].set_key(KEY_B, entry_id="entry-1")
        self.bridge.observe("s1")
        record = self.record()
        self.assertEqual(record["route"]["epoch"], 2)
        self.assertIsNone(record["quota"])

    def test_a_provider_switch_records_an_unsupported_route_and_never_fetches_for_it(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        agent = self.agents["s1"]
        agent.provider, agent.api_mode = "anthropic", "anthropic_messages"
        agent.base_url = agent._client_kwargs["base_url"] = "https://api.anthropic.com"
        self.bridge.tick()  # idle /model: only the poller sees it
        record = self.record()
        self.assertEqual(record["route"], dict(record["route"], epoch=2, provider="anthropic",
                                               supported=False, identity=None))
        self.assertIsNone(record["quota"])
        self.advance(600)
        self.bridge.observe("s1", activity=True)
        self.bridge.tick()
        self.assertEqual(len(self.fetched), 1)
        self.assertIsNone(self.record()["quota"])

    def test_a_model_change_inside_one_credential_keeps_the_quota_and_nudges_the_sidebar(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        before = self.record()
        self.advance(5)
        self.agents["s1"].model = "model-b"
        self.bridge.tick()
        self.assertEqual(self.record(), before)
        self.assertEqual(len(self.notified), 2)

    def test_a_failed_fetch_backs_off_and_keeps_the_same_epochs_reading(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        good = self.record()["quota"]
        self.answer = None
        attempts = []
        for _ in range(40):
            self.advance(30)
            self.bridge.observe("s1", activity=True)
            before = len(self.fetched)
            self.bridge.tick()
            if len(self.fetched) != before:
                attempts.append(self.clock[0])
        gaps = [later - earlier for earlier, later in zip(attempts, attempts[1:])]
        self.assertEqual(gaps, sorted(gaps))
        self.assertGreaterEqual(gaps[0], 120)
        self.assertLessEqual(max(gaps), bridge_module.BACKOFF_MAX_SECONDS + 30)
        self.assertEqual(self.record()["quota"], good)

    def test_an_agent_that_disappears_is_unknown_at_once_and_forgotten_later(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        del self.agents["s1"]  # /new rotated the id, or the surface dropped the session
        self.bridge.tick()
        record = self.record()
        self.assertEqual((record["route"]["provider"], record["route"]["supported"]), (None, False))
        self.assertIsNone(record["quota"])
        for _ in range(bridge_module.MISSING_TICKS_BEFORE_DROP):
            self.bridge.tick()
        self.assertEqual(sorted(os.listdir(self.dir)), [])

    def test_a_subagent_or_unknown_session_creates_nothing(self):
        for session_id in ("child-session", "../escape", "", "a" * 129, None, 7):
            self.bridge.observe(session_id)
        self.assertEqual(os.listdir(self.dir), [])

    def test_finalize_and_close_remove_the_mailbox(self):
        self.agents["s2"] = FakeAgent("s2", KEY_B)
        self.bridge.observe("s1")
        self.bridge.observe("s2")
        self.bridge.tick()
        self.bridge.finalize("s1")
        self.assertEqual(sorted(os.listdir(self.dir)), ["s2.json", "s2.lock"])
        self.bridge.finalize(None)
        self.bridge.close()
        self.assertEqual(os.listdir(self.dir), [])
        self.bridge.observe("s1")  # after unload nothing is tracked again
        self.assertEqual(os.listdir(self.dir), [])

    def test_a_forked_child_exiting_leaves_the_parents_mailbox_alone(self):
        self.bridge.observe("s1")
        pid = os.fork()
        if pid == 0:  # the child: its atexit hook runs close(); exit without unittest's teardown
            try:
                self.bridge.close()
            finally:
                os._exit(0)
        os.waitpid(pid, 0)
        self.assertEqual(sorted(os.listdir(self.dir)), ["s1.json", "s1.lock"])
        self.bridge.close()
        self.assertEqual(os.listdir(self.dir), [])

    def test_the_lock_is_held_for_as_long_as_the_producer_tracks_the_session(self):
        self.bridge.observe("s1")
        reader = os.open(self.dir / "s1.lock", os.O_RDONLY)
        try:
            with self.assertRaises(OSError):
                fcntl.flock(reader, fcntl.LOCK_SH | fcntl.LOCK_NB)
            self.bridge.finalize("s1")
            fcntl.flock(reader, fcntl.LOCK_SH | fcntl.LOCK_NB)  # the producer is gone
        finally:
            os.close(reader)

    def test_a_second_producer_cannot_write_a_session_another_one_owns(self):
        other = self.make_bridge()
        try:
            self.bridge.observe("s1")
            self.bridge.tick()
            owned = self.record()
            other_agent = FakeAgent("s1", KEY_B)
            other._find_agent = lambda _session_id: other_agent
            other.observe("s1", activity=True)
            other.tick()
            self.assertEqual(self.record(), owned)
            self.assertEqual(len(self.fetched), 1)
            # Once the owner lets go, the other process may take the session over.
            self.bridge.finalize("s1")
            other.observe("s1")
            self.assertEqual(self.record()["route"]["epoch"], 1)
            self.assertNotEqual(self.record()["route"]["identity"], owned["route"]["identity"])
        finally:
            other.close()

    def test_a_mailbox_that_cannot_be_rewritten_gives_up_its_lock(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        os.chmod(self.dir, 0o500)
        try:
            self.agents["s1"].set_key(KEY_B)
            self.bridge.observe("s1")  # the invalidation cannot be written
            reader = os.open(self.dir / "s1.lock", os.O_RDONLY)
            try:
                # The stale quota is still on disk, so the reader must see no live producer.
                fcntl.flock(reader, fcntl.LOCK_SH | fcntl.LOCK_NB)
            finally:
                os.close(reader)
        finally:
            os.chmod(self.dir, 0o700)

    def test_a_mailbox_that_went_away_is_retried_without_nudging_every_tick(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        nudges = len(self.notified)
        os.chmod(self.dir, 0o500)  # an uninstall while this Hermes keeps running
        try:
            for _ in range(20):
                self.advance(bridge_module.ROUTE_TTL_SECONDS + 1)
                self.bridge.tick()
            self.assertEqual(len(self.notified), nudges)
        finally:
            os.chmod(self.dir, 0o700)


class LateAnswerTests(BridgeCase):
    def make_gated_bridge(self):
        self.gate = threading.Event()
        self.started = threading.Event()
        self.threads = []

        def slow_fetch(base_url, api_key):
            self.fetched.append((base_url, api_key))
            self.started.set()
            self.gate.wait(10)
            return [{"kind": "7d", "used_percent": 99.0, "resets_at": None}]

        def spawn(target, *args):
            if target.__name__ == "_poll_loop":
                return Inline(target, args)
            thread = threading.Thread(target=target, args=args, daemon=True)
            self.threads.append(thread)
            return thread

        self.bridge.close()
        self.bridge = self.make_bridge(fetch_windows=slow_fetch, spawn_thread=spawn)

    def finish(self):
        self.gate.set()
        for thread in self.threads:
            thread.join(10)
            self.assertFalse(thread.is_alive())

    def test_an_answer_for_a_rotated_credential_is_discarded(self):
        self.make_gated_bridge()
        self.bridge.observe("s1")
        self.bridge.tick()
        self.assertTrue(self.started.wait(10))
        self.agents["s1"].set_key(KEY_B)
        self.bridge.observe("s1")
        self.finish()
        record = self.record()
        self.assertEqual(record["route"]["epoch"], 2)
        self.assertIsNone(record["quota"])

    def test_an_answer_is_discarded_when_the_rotation_is_only_seen_at_completion(self):
        self.make_gated_bridge()
        self.bridge.observe("s1")
        self.bridge.tick()
        self.assertTrue(self.started.wait(10))
        self.agents["s1"].set_key(KEY_B)  # nobody observed it yet
        self.finish()
        record = self.record()
        self.assertEqual(record["route"]["epoch"], 2)
        self.assertIsNone(record["quota"])

    def test_an_answer_is_discarded_even_when_the_route_went_away_and_came_back(self):
        self.make_gated_bridge()
        self.bridge.observe("s1")
        self.bridge.tick()
        self.assertTrue(self.started.wait(10))
        self.agents["s1"].set_key(KEY_B)
        self.bridge.observe("s1")
        self.agents["s1"].set_key(KEY_A)
        self.bridge.observe("s1")
        self.finish()
        record = self.record()
        self.assertEqual(record["route"]["epoch"], 3)
        self.assertIsNone(record["quota"])

    def test_an_answer_for_a_finalized_session_is_discarded(self):
        self.make_gated_bridge()
        self.bridge.observe("s1")
        self.bridge.tick()
        self.assertTrue(self.started.wait(10))
        self.bridge.finalize("s1")
        self.finish()
        self.assertEqual(os.listdir(self.dir), [])

    def test_a_stuck_request_is_abandoned_but_no_second_worker_joins_it(self):
        self.make_gated_bridge()
        self.agents["s2"] = FakeAgent("s2", KEY_B)
        self.bridge.observe("s1")
        self.bridge.observe("s2")
        self.bridge.tick()
        self.assertTrue(self.started.wait(10))
        self.bridge.tick()
        self.assertEqual(len(self.fetched), 1)
        # Hours of turns and ticks, every backoff long expired, while the worker hangs.
        for _ in range(50):
            self.advance(bridge_module.BACKOFF_MAX_SECONDS + 1)
            self.bridge.observe("s1", activity=True)
            self.bridge.observe("s2", activity=True)
            self.bridge.tick()
        self.assertEqual(len(self.threads), 1, "a hung worker was joined by another")
        self.assertEqual(len(self.fetched), 1)
        self.assertTrue(self.threads[0].is_alive())

        # It returns at last: past its deadline, so the answer is not written.
        self.finish()
        stuck = "s1" if self.fetched[0][1] == KEY_A else "s2"
        self.assertIsNone(self.record(stuck)["quota"])
        # Only now is there room for the next request.
        self.advance(bridge_module.BACKOFF_MAX_SECONDS + 1)
        self.bridge.tick()
        deadline = time.time() + 10
        while len(self.fetched) < 2 and time.time() < deadline:
            time.sleep(0.01)
        self.assertEqual(len(self.fetched), 2)
        self.assertEqual(len(self.threads), 2)
        self.finish()


class PollerThreadTests(BridgeCase):
    def test_a_real_poller_follows_an_idle_switch_and_stops_on_close(self):
        self.bridge.close()
        self.bridge = self.make_bridge(
            spawn_thread=bridge_module._plain_thread, poll_seconds=0.01, now=time.time
        )
        self.bridge.observe("s1")
        poller = self.bridge._poller
        self.assertTrue(poller.is_alive())
        deadline = time.time() + 10
        while not self.fetched and time.time() < deadline:
            time.sleep(0.01)
        self.assertEqual(self.fetched, [(CODEX_BASE, KEY_A)])
        self.agents["s1"].set_key(KEY_B)  # no hook fires: an idle switch
        deadline = time.time() + 10
        while self.record()["route"]["epoch"] != 2 and time.time() < deadline:
            time.sleep(0.01)
        self.assertEqual(self.record()["route"]["epoch"], 2)
        # A's reading is gone; the only quota this epoch may carry is B's own.
        quota = self.record()["quota"]
        if quota is not None:
            self.assertEqual((quota["epoch"], quota["identity"]), (2, self.record()["route"]["identity"]))
            self.assertEqual(self.fetched[-1], (CODEX_BASE, KEY_B))
        self.bridge.close()
        poller.join(10)
        self.assertFalse(poller.is_alive())
        self.assertEqual(os.listdir(self.dir), [])


class FetcherTests(unittest.TestCase):
    def setUp(self):
        self.calls = []
        self.snapshot = None
        agent_package = types.ModuleType("agent")
        account_usage = types.ModuleType("agent.account_usage")

        def fetch_account_usage(provider, *, base_url=None, api_key=None):
            self.calls.append((provider, base_url, api_key))
            return self.snapshot

        account_usage.fetch_account_usage = fetch_account_usage
        self._saved = {name: sys.modules.get(name) for name in ("agent", "agent.account_usage")}
        sys.modules["agent"], sys.modules["agent.account_usage"] = agent_package, account_usage

    def tearDown(self):
        for name, module in self._saved.items():
            if module is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = module

    def window(self, label, used, reset_at=None):
        return types.SimpleNamespace(label=label, used_percent=used, reset_at=reset_at, detail="x")

    def test_the_fetcher_is_called_with_exactly_the_live_key_and_only_plain_windows_leave(self):
        import datetime

        reset = datetime.datetime(2099, 1, 1, tzinfo=datetime.timezone.utc)
        self.snapshot = types.SimpleNamespace(
            provider="openai-codex", unavailable_reason=None, plan="Plus", title="Account limits",
            details=["Credits balance: $1.00"], raw={"email": "user@example.com"},
            windows=[
                self.window("Session", 25, reset),
                self.window("Weekly", 130.0),
                self.window("Weekly", 1.0),
                self.window("Opus week", 50.0),
                self.window("API key quota", 40.0),
                self.window("Session", float("nan")),
                self.window("Weekly", True),
            ],
        )
        windows = bridge_module.fetch_codex_windows(CODEX_BASE, KEY_A)
        self.assertEqual(self.calls, [("openai-codex", CODEX_BASE, KEY_A)])
        self.assertEqual(windows, [
            {"kind": "5h", "used_percent": 25.0, "resets_at": 4_070_908_800},
            {"kind": "7d", "used_percent": 100.0, "resets_at": None},
        ])

    def test_no_answer_another_provider_or_an_unavailable_account_is_no_reading(self):
        self.assertIsNone(bridge_module.fetch_codex_windows(CODEX_BASE, KEY_A))
        week = [self.window("Weekly", 1.0)]
        for snapshot in (
            types.SimpleNamespace(provider="anthropic", unavailable_reason=None, windows=week),
            types.SimpleNamespace(provider="openai-codex", unavailable_reason="no", windows=week),
            types.SimpleNamespace(provider="openai-codex", unavailable_reason=None, windows=[]),
        ):
            self.snapshot = snapshot
            self.assertIsNone(bridge_module.fetch_codex_windows(CODEX_BASE, KEY_A))


class AdapterTests(unittest.TestCase):
    def setUp(self):
        self._saved = {name: sys.modules.get(name) for name in ("hermes_cli.plugins", "tui_gateway.server")}

    def tearDown(self):
        for name, module in self._saved.items():
            if module is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = module

    def install(self, cli_agent=None, tui_sessions=None):
        plugins = types.ModuleType("hermes_cli.plugins")
        manager = types.SimpleNamespace(_cli_ref=types.SimpleNamespace(agent=cli_agent))
        plugins.get_plugin_manager = lambda: manager
        sys.modules["hermes_cli.plugins"] = plugins
        if tui_sessions is not None:
            server = types.ModuleType("tui_gateway.server")
            server._sessions = tui_sessions
            sys.modules["tui_gateway.server"] = server

    def test_the_cli_agent_and_tui_sessions_are_found_by_exact_session_id(self):
        cli, tui = FakeAgent("cli-session"), FakeAgent("tui-session")
        self.install(cli, {"ui-1": {"agent": tui, "session_key": "tui-session"}, "ui-2": {"agent": None}})
        self.assertIs(bridge_module.find_live_agent("cli-session"), cli)
        self.assertIs(bridge_module.find_live_agent("tui-session"), tui)
        self.assertIsNone(bridge_module.find_live_agent("ui-1"))
        self.assertIsNone(bridge_module.find_live_agent("child-session"))

    def test_two_agents_claiming_one_session_is_not_a_coin_flip(self):
        self.install(None, {"a": {"agent": FakeAgent("s1")}, "b": {"agent": FakeAgent("s1", KEY_B)}})
        self.assertIsNone(bridge_module.find_live_agent("s1"))

    def test_missing_or_reshaped_internals_yield_no_agent(self):
        sys.modules.pop("hermes_cli.plugins", None)
        sys.modules.pop("tui_gateway.server", None)
        self.assertEqual(bridge_module._live_agents(), [])
        broken = types.ModuleType("hermes_cli.plugins")  # no get_plugin_manager
        server = types.ModuleType("tui_gateway.server")
        server._sessions = ["not", "a", "dict"]
        sys.modules["hermes_cli.plugins"], sys.modules["tui_gateway.server"] = broken, server
        self.assertEqual(bridge_module._live_agents(), [])


class InstallTests(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self.state = self.root / "state"
        self.state.mkdir(mode=0o700)
        self.config = self.root / "bridge.json"
        self.write_config({"state_dir": str(self.state), "executable": "/usr/bin/true"})
        memory_provider = types.ModuleType("agent.memory_provider")
        memory_provider.spawn_context_thread = lambda target, *, name, args=(): threading.Thread(
            target=target, args=args, name=name, daemon=True
        )
        self._saved = {name: sys.modules.get(name) for name in ("agent", "agent.memory_provider")}
        sys.modules["agent"] = types.ModuleType("agent")
        sys.modules["agent.memory_provider"] = memory_provider
        self.environ = {"HERDR_ENV": "1", "HERDR_PANE_ID": "w4C:p1"}
        self.ctx = types.SimpleNamespace(profile_name="default")

    def tearDown(self):
        for name, module in self._saved.items():
            if module is None:
                sys.modules.pop(name, None)
            else:
                sys.modules[name] = module
        self._tmp.cleanup()

    def write_config(self, value, mode=0o600):
        self.config.write_text(json.dumps(value))
        os.chmod(self.config, mode)

    def build(self, **overrides):
        options = dict(config_path=self.config, environ=self.environ)
        options.update(overrides)
        bridge = bridge_module.build_bridge(self.ctx, **options)
        if bridge is not None:
            self.addCleanup(bridge.close)
        return bridge

    def test_the_bridge_is_built_only_inside_a_default_profile_herdr_pane(self):
        self.assertIsNotNone(self.build())
        mailbox = self.state / "hermes-bridge"
        self.assertEqual(stat.S_IMODE(os.stat(mailbox).st_mode), 0o700)
        self.assertIsNone(self.build(environ={"HERDR_PANE_ID": "w4C:p1"}))
        self.assertIsNone(self.build(environ={"HERDR_ENV": "1"}))
        self.assertIsNone(self.build(environ={"HERDR_ENV": "1", "HERDR_PANE_ID": "../x"}))
        for profile in ("work", "custom", None):
            self.ctx.profile_name = profile
            self.assertIsNone(self.build(), profile)

    def test_a_bridge_config_someone_else_could_write_is_refused(self):
        self.write_config({"state_dir": str(self.state), "executable": "/usr/bin/true"}, mode=0o666)
        self.assertIsNone(self.build())
        self.write_config({"state_dir": "relative/state", "executable": "/usr/bin/true"})
        self.assertIsNone(self.build())
        self.write_config({"state_dir": str(self.state)})
        self.assertIsNone(self.build())
        self.write_config(["not", "an", "object"])
        self.assertIsNone(self.build())
        self.config.unlink()
        self.assertIsNone(self.build())
        real = self.root / "real.json"
        real.write_text(json.dumps({"state_dir": str(self.state), "executable": "/usr/bin/true"}))
        os.chmod(real, 0o600)
        self.config.symlink_to(real)
        self.assertIsNone(self.build())

    def test_the_mailbox_is_never_a_symlink_or_a_shared_directory(self):
        elsewhere = self.root / "elsewhere"
        elsewhere.mkdir()
        (self.state / "hermes-bridge").symlink_to(elsewhere)
        self.assertIsNone(self.build())
        (self.state / "hermes-bridge").unlink()
        (self.state / "hermes-bridge").mkdir(mode=0o755)
        os.chmod(self.state / "hermes-bridge", 0o755)
        self.assertIsNotNone(self.build())
        self.assertEqual(stat.S_IMODE(os.stat(self.state / "hermes-bridge").st_mode), 0o700)
        os.chmod(self.state, 0o777)
        self.assertIsNone(self.build())
        os.chmod(self.state, 0o700)

    def test_a_dead_producers_files_are_pruned_and_a_live_ones_are_kept(self):
        mailbox = self.state / "hermes-bridge"
        mailbox.mkdir(mode=0o700)
        old = time.time() - 2 * 60 * 60
        for name in ("dead.json", "dead.lock", "live.json", "live.lock", "orphan.json", ".x.1.tmp", "new.json"):
            (mailbox / name).write_text("{}")
            if name != "new.json":
                os.utime(mailbox / name, (old, old))
        live = os.open(mailbox / "live.lock", os.O_RDWR)
        fcntl.flock(live, fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            self.assertIsNotNone(self.build())
            self.assertEqual(sorted(os.listdir(mailbox)), ["live.json", "live.lock", "new.json"])
        finally:
            os.close(live)

    def test_register_wires_the_hooks_and_ignores_non_interactive_platforms(self):
        hooks, unload = {}, []
        self.ctx.register_hook = lambda name, callback: hooks.setdefault(name, callback)
        self.ctx.on_unload = unload.append
        saved = (os.environ.get("HERDR_ENV"), os.environ.get("HERDR_PANE_ID"))
        real_config = bridge_module.Path(bridge_module.__file__).with_name("bridge.json")
        self.assertFalse(real_config.exists(), "the repository copy must not carry a bridge.json")
        os.environ.update(self.environ)
        original = bridge_module.build_bridge
        built = []

        def build(ctx):
            bridge = original(ctx, config_path=self.config)
            bridge._find_agent = lambda session_id: agents.get(session_id)
            bridge._fetch_windows = lambda *_secret: None
            bridge._spawn_thread = lambda target, *args: Inline(target, args)
            built.append(bridge)
            return bridge

        agents = {"s1": FakeAgent()}
        bridge_module.build_bridge = build
        try:
            bridge_module.register(self.ctx)
        finally:
            bridge_module.build_bridge = original
            for name, value in zip(("HERDR_ENV", "HERDR_PANE_ID"), saved):
                if value is None:
                    os.environ.pop(name, None)
                else:
                    os.environ[name] = value
        self.addCleanup(built[0].close)
        self.assertEqual(sorted(hooks), [
            "on_session_finalize", "on_session_reset", "on_session_start", "post_api_request",
            "pre_api_request", "pre_llm_call",
        ])
        self.assertEqual(unload, [built[0].close])
        mailbox = self.state / "hermes-bridge"
        conversation = "the user said something private"
        hooks["pre_api_request"](session_id="s1", platform="telegram", user_message=conversation)
        hooks["pre_api_request"](session_id=None, platform="cli")
        self.assertEqual(os.listdir(mailbox), [])
        hooks["pre_api_request"](session_id="s1", platform="tui", user_message=conversation,
                                 conversation_history=[conversation], request_messages=[conversation])
        self.assertEqual(sorted(os.listdir(mailbox)), ["s1.json", "s1.lock"])
        for name in os.listdir(mailbox):
            data = (mailbox / name).read_bytes()
            self.assertNotIn(conversation.encode(), data)
            self.assertNotIn(KEY_A.encode(), data)
        hooks["on_session_finalize"](session_id="s1", platform="tui")
        self.assertEqual(os.listdir(mailbox), [])

    def test_the_notifier_passes_the_state_dir_without_touching_the_environment(self):
        out = self.root / "notified"
        script = self.root / "notify.sh"
        script.write_text('#!/bin/sh\nprintf "%s|%s" "$1" "$HERDR_PLUGIN_STATE_DIR" > "{}"\n'.format(out))
        os.chmod(script, 0o755)
        before = dict(os.environ)
        notifier = bridge_module._Notifier(str(script), str(self.state))
        notifier()
        deadline = time.time() + 10
        while not (out.exists() and out.read_text()) and time.time() < deadline:
            time.sleep(0.01)
        self.assertEqual(out.read_text(), "hermes-notify|{}".format(self.state))
        self.assertEqual(dict(os.environ), before)
        for child, _started in notifier._children:
            child.wait(10)


class NotifyRateTests(BridgeCase):
    def test_nudges_are_coalesced(self):
        self.bridge.observe("s1")
        self.bridge.tick()
        self.assertEqual(len(self.notified), 1)
        self.agents["s1"].model = "model-b"
        self.bridge.tick()
        self.assertEqual(len(self.notified), 1)  # inside the minimum interval: still pending
        self.advance(bridge_module.NOTIFY_MIN_INTERVAL_SECONDS)
        self.bridge.tick()
        self.assertEqual(len(self.notified), 2)
        self.advance(60)
        self.bridge.tick()
        self.assertEqual(len(self.notified), 2)  # nothing changed, nothing sent


if __name__ == "__main__":
    unittest.main(verbosity=1)
