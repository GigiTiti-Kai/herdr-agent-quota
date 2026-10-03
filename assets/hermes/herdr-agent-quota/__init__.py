"""Hermes plugin installed by herdr-agent-quota: live-session account limits for the Herdr sidebar.

managed by herdr-agent-quota; `configure` replaces this file.

Hermes does not record which credential serves a session, so a quota fetched for "whichever
credential a new session would pick" cannot be attributed to a running one. This plugin runs
inside the Hermes process, where the live agent is, and asks Hermes's own account-usage fetcher
about exactly the key that agent holds. The result goes to a per-session mailbox file the quota
plugin reads. Nothing secret is written: no token, no email, no credential label, no provider
response.

Scope, on purpose:

* default profile only, interactive CLI/TUI sessions only, inside a Herdr pane only;
* `openai-codex` over `https://chatgpt.com/backend-api` only. Anything else is recorded as an
  unsupported route with no quota, never asked about through another credential;
* any change of provider, endpoint, pool entry, or token starts a new epoch and drops the quota
  before anything else happens. A usage answer that arrives for an older epoch is discarded.

Hermes has no public way to reach the live agent from a hook, so `_live_agents` reads two private
attributes. They are confined to that one function; when they are missing the plugin knows
nothing and writes no quota.
"""

# HERDR_AGENT_QUOTA_BRIDGE_VERSION=1

from __future__ import annotations

import atexit
import hashlib
import hmac
import json
import math
import os
import re
import secrets
import stat
import subprocess
import sys
import threading
import time
from pathlib import Path
from urllib.parse import urlsplit

try:  # POSIX only; without flock there is no liveness proof, so the plugin stays inert.
    import fcntl
except ImportError:  # pragma: no cover - Windows
    fcntl = None

SCHEMA = 1
MAILBOX_DIR = "hermes-bridge"
SUPPORTED_PROVIDER = "openai-codex"
SUPPORTED_API_MODE = "codex_responses"
SUPPORTED_HOST = "chatgpt.com"
SUPPORTED_PATH = "/backend-api"
INTERACTIVE_PLATFORMS = frozenset({"cli", "tui"})
# Hermes's labels for the two plain Codex pools. Anything else is not a window.
WINDOW_KINDS = {"session": "5h", "weekly": "7d"}

POLL_SECONDS = 2.0
# The reader stops trusting a route nobody re-checked for ROUTE_TTL_SECONDS (its constant; kept
# equal here). Every check re-stamps the record at most this often.
HEARTBEAT_SECONDS = 30.0
ROUTE_TTL_SECONDS = 120.0
DEBOUNCE_SECONDS = 60.0
BACKOFF_MAX_SECONDS = 15 * 60.0
FETCH_DEADLINE_SECONDS = 45.0
NOTIFY_MIN_INTERVAL_SECONDS = 2.0
NOTIFY_CHILD_SECONDS = 15.0
# Ticks an untracked agent is tolerated before its session is forgotten (about a minute).
MISSING_TICKS_BEFORE_DROP = 30
# Credentials one session remembers a debounce for. A pool rarely holds more; past this the
# memory is dropped, which only lets an old entry be asked once more.
MAX_REMEMBERED_ATTEMPTS = 16
MAX_BRIDGE_CONFIG_BYTES = 4096
PRUNE_AFTER_SECONDS = 60 * 60.0

_SESSION_ID_RE = re.compile(r"^[A-Za-z0-9_-]{1,128}$")
_PANE_ID_RE = re.compile(r"^[A-Za-z0-9_:.-]{1,64}$")
_MISSING = object()


class _Signature(tuple):
    """What identifies a route for epoch purposes: (provider, supported, identity)."""

    __slots__ = ()

    provider = property(lambda self: self[0])
    supported = property(lambda self: self[1])
    identity = property(lambda self: self[2])


_UNKNOWN = _Signature((None, False, None))


def _read_once(agent):
    kwargs = getattr(agent, "_client_kwargs", None)
    has_kwargs = isinstance(kwargs, dict)
    return (
        getattr(agent, "provider", None),
        getattr(agent, "api_mode", None),
        getattr(agent, "base_url", None),
        getattr(agent, "api_key", None),
        getattr(agent, "_credential_pool_entry_id", None),
        kwargs.get("api_key", _MISSING) if has_kwargs else _MISSING,
        kwargs.get("base_url", _MISSING) if has_kwargs else _MISSING,
    )


def _is_supported_endpoint(base_url):
    try:
        parts = urlsplit(base_url)
        return (
            parts.scheme == "https"
            and parts.hostname == SUPPORTED_HOST
            and parts.port in (None, 443)
            and not parts.username
            and not parts.password
            and (parts.path == SUPPORTED_PATH or parts.path.startswith(SUPPORTED_PATH + "/"))
        )
    except ValueError:
        return False


def read_route(agent, hmac_key):
    """One consistent view of the live route: `(signature, model, (base_url, api_key) | None)`.

    The key, the endpoint, and the client that actually sends requests are read together and read
    twice. A rotation swaps them one attribute at a time, so two reads that differ, or an agent
    whose client disagrees with it, is a route in motion: unknown, not a guess.
    """
    first = _read_once(agent)
    if first != _read_once(agent):
        return _UNKNOWN, None, None
    provider, api_mode, base_url, api_key, entry_id, client_key, client_base = first
    model = getattr(agent, "model", None)
    model = model if isinstance(model, str) else None
    if not isinstance(provider, str) or not provider.strip():
        return _UNKNOWN, model, None
    provider = provider.strip().lower()
    unsupported = _Signature((provider, False, None))
    # A rotating key source is installed as a callable; there is no single key to speak for.
    if not isinstance(api_key, str) or not api_key.strip() or not isinstance(base_url, str):
        return unsupported, model, None
    if provider != SUPPORTED_PROVIDER or api_mode != SUPPORTED_API_MODE:
        return unsupported, model, None
    if client_key is _MISSING or client_base is _MISSING or client_key != api_key:
        return unsupported, model, None
    if not isinstance(client_base, str) or client_base.rstrip("/") != base_url.rstrip("/"):
        return unsupported, model, None
    # Hermes's usage fetcher would follow any base URL it is handed. Only the ChatGPT backend
    # is a place this token may be sent for a usage read.
    if not _is_supported_endpoint(base_url):
        return unsupported, model, None
    entry = entry_id if isinstance(entry_id, str) else ""
    material = "\0".join((provider, api_mode, base_url.rstrip("/"), entry, api_key))
    identity = hmac.new(hmac_key, material.encode("utf-8"), hashlib.sha256).hexdigest()
    return _Signature((provider, True, identity)), model, (base_url, api_key)


def _live_agents():
    """Every top-level agent this process hosts.

    The only place private Hermes attributes are read: the interactive CLI registers itself on
    the plugin manager (`_cli_ref`), and the TUI gateway keeps its sessions in
    `tui_gateway.server._sessions`. Neither module is imported here — a surface that is not
    already running has no agent. A change upstream yields nothing, which reads as unknown.
    """
    agents = []
    plugins = sys.modules.get("hermes_cli.plugins")
    try:
        manager = plugins.get_plugin_manager() if plugins is not None else None
        agent = getattr(getattr(manager, "_cli_ref", None), "agent", None)
        if agent is not None:
            agents.append(agent)
    except Exception:
        pass
    server = sys.modules.get("tui_gateway.server")
    sessions = getattr(server, "_sessions", None)
    if isinstance(sessions, dict):
        try:
            records = list(sessions.values())
        except RuntimeError:  # resized while copying; the next tick reads it again
            records = []
        for record in records:
            agent = record.get("agent") if isinstance(record, dict) else None
            if agent is not None and all(agent is not known for known in agents):
                agents.append(agent)
    return agents


def find_live_agent(session_id):
    """The one live agent whose session id is exactly `session_id`, else None."""
    matches = [
        agent for agent in _live_agents() if str(getattr(agent, "session_id", "") or "") == session_id
    ]
    return matches[0] if len(matches) == 1 else None


def fetch_codex_windows(base_url, api_key):
    """Ask Hermes's own fetcher about exactly this key. Returns whitelisted windows or None.

    With an explicit key the fetcher uses that key, and on a 401 refreshes that same credential
    rather than selecting another one. Only the plain 5h/7d pools leave this function; the
    provider payload, plan, and detail lines stay behind.
    """
    from agent.account_usage import fetch_account_usage

    snapshot = fetch_account_usage(SUPPORTED_PROVIDER, base_url=base_url, api_key=api_key)
    if snapshot is None or getattr(snapshot, "provider", None) != SUPPORTED_PROVIDER:
        return None
    if getattr(snapshot, "unavailable_reason", None):
        return None
    windows = []
    for window in getattr(snapshot, "windows", None) or ():
        kind = WINDOW_KINDS.get(str(getattr(window, "label", "")).strip().lower())
        used = getattr(window, "used_percent", None)
        if kind is None or isinstance(used, bool) or not isinstance(used, (int, float)):
            continue
        if not math.isfinite(used) or any(existing["kind"] == kind for existing in windows):
            continue
        reset_at = getattr(window, "reset_at", None)
        try:
            resets_at = int(reset_at.timestamp()) if reset_at is not None else None
        except (AttributeError, OverflowError, OSError, ValueError):
            resets_at = None
        windows.append(
            {
                "kind": kind,
                "used_percent": min(100.0, max(0.0, float(used))),
                "resets_at": resets_at if resets_at is not None and resets_at >= 0 else None,
            }
        )
    return windows or None


class _Session:
    __slots__ = (
        "session_id", "lock_fd", "epoch", "signature", "model", "quota", "wants_fetch",
        "next_fetch_at", "failures", "fetch_token", "fetch_started", "missing_ticks", "written_at",
        "attempts",
    )

    def __init__(self, session_id, lock_fd):
        self.session_id = session_id
        self.lock_fd = lock_fd
        self.epoch = 0
        self.signature = None
        self.model = None
        self.quota = None
        self.wants_fetch = False
        self.next_fetch_at = 0.0
        self.failures = 0
        self.fetch_token = None
        self.fetch_started = 0.0
        self.missing_ticks = 0
        self.written_at = 0.0
        self.attempts = {}  # identity -> (next_fetch_at, failures) of credentials not in use


class Bridge:
    """Tracks the live route of each session and publishes its quota to the mailbox.

    Hook callbacks only compare attributes and, on a change, rewrite one small file. Network
    calls and subprocesses happen on the bridge's own threads.
    """

    def __init__(self, mailbox_dir, pane_id, *, find_agent=find_live_agent,
                 fetch_windows=fetch_codex_windows, notify=None, now=time.time,
                 spawn_thread=None, poll_seconds=POLL_SECONDS):
        self._dir = Path(mailbox_dir)
        self._pane_id = pane_id
        self._find_agent = find_agent
        self._fetch_windows = fetch_windows
        self._notify = notify
        self._now = now
        self._spawn_thread = spawn_thread or _plain_thread
        self._poll_seconds = poll_seconds
        # Never written anywhere: identities from two processes are not comparable, and a
        # mailbox identity cannot be linked back to a token.
        self._hmac_key = secrets.token_bytes(32)
        self._pid = os.getpid()
        self._lock = threading.RLock()
        self._sessions = {}
        self._fetch_in_flight = None
        self._notify_pending = False
        self._last_notify = 0.0
        self._stop = threading.Event()
        self._poller = None
        self._closed = False

    # ---------------------------------------------------------------- hook entry points

    def observe(self, session_id, activity=False):
        """Reconcile one session with its live agent. Cheap; safe to call from a hook."""
        if not isinstance(session_id, str) or not _SESSION_ID_RE.fullmatch(session_id):
            return
        with self._lock:
            if self._closed:
                return
            session = self._sessions.get(session_id)
            agent = self._find_agent(session_id)
            if session is None:
                # Subagents fire hooks with their own ids; only a top-level agent is tracked.
                if agent is None:
                    return
                lock_fd = self._acquire_lock(session_id)
                if lock_fd is None:  # another live process owns this session's mailbox
                    return
                session = self._sessions[session_id] = _Session(session_id, lock_fd)
            self._reconcile(session, agent)
            if activity and session.signature is not None and session.signature.supported:
                session.wants_fetch = True
        self._ensure_poller()

    def finalize(self, session_id):
        """The session is being torn down: release its mailbox."""
        with self._lock:
            session = self._sessions.pop(session_id, None) if isinstance(session_id, str) else None
            if session is not None:
                self._release(session)
                self._notify_pending = True

    def close(self):
        """Unload or process exit: stop the poller and release every mailbox."""
        if os.getpid() != self._pid:
            # A forked child inherits the atexit hook, and maybe a lock held mid-tick. The
            # mailbox belongs to the parent, which is still running.
            return
        with self._lock:
            if self._closed:
                return
            self._closed = True
            self._stop.set()
            for session in list(self._sessions.values()):
                self._release(session)
            self._sessions.clear()

    # ---------------------------------------------------------------- reconciliation

    def _reconcile(self, session, agent):
        if agent is None:
            signature, model = _UNKNOWN, None
            session.missing_ticks += 1
        else:
            signature, model, _secret = read_route(agent, self._hmac_key)
            session.missing_ticks = 0
        if signature != session.signature:
            # Debounce and backoff belong to the credential that was asked, not the session:
            # a pool that moves on to a fresh entry is asked about it at once, and coming back
            # to an entry that was failing does not reset that entry's wait.
            old = session.signature.identity if session.signature is not None else None
            if signature.identity != old:
                if old is not None:
                    session.attempts[old] = (session.next_fetch_at, session.failures)
                if signature.identity is not None:
                    session.next_fetch_at, session.failures = session.attempts.pop(
                        signature.identity, (0.0, 0)
                    )
                if len(session.attempts) > MAX_REMEMBERED_ATTEMPTS:
                    session.attempts.clear()
            # A new epoch: whatever was known about the previous credential is gone before
            # anything is fetched for the new one.
            session.epoch += 1
            session.signature = signature
            session.quota = None
            session.fetch_token = None  # an answer still in flight belongs to the old epoch
            session.wants_fetch = signature.supported
            session.model = model
            self._write(session)
            self._notify_pending = True
        elif model != session.model:
            # The label comes from Hermes's own session row; only the sidebar needs a nudge.
            session.model = model
            self._notify_pending = True
        quiet_for = self._now() - session.written_at
        if quiet_for >= HEARTBEAT_SECONDS:
            # The route was just checked: say so, so the reader can tell a watched session from
            # one whose poller died. No nudge: a heartbeat changes nothing the sidebar shows,
            # unless the reader already gave up on this record while nobody was checking. A
            # record that cannot be written is retried here, silently.
            if self._write(session) and quiet_for > ROUTE_TTL_SECONDS:
                self._notify_pending = True

    def tick(self):
        """One poller pass: follow idle `/model` switches, start due fetches, send nudges."""
        with self._lock:
            if self._closed:
                return
            now = self._now()
            self._expire_hung_fetch(now)
            for session in list(self._sessions.values()):
                self._reconcile(session, self._find_agent(session.session_id))
                if session.missing_ticks >= MISSING_TICKS_BEFORE_DROP:
                    del self._sessions[session.session_id]
                    self._release(session)
                    continue
                self._maybe_start_fetch(session, now)
        self._flush_notify()

    def _maybe_start_fetch(self, session, now):
        if not session.wants_fetch or self._fetch_in_flight is not None or now < session.next_fetch_at:
            return
        agent = self._find_agent(session.session_id)
        if agent is None:
            return
        signature, _model, secret = read_route(agent, self._hmac_key)
        # The secret pair comes from the same read as the signature it is fetched under.
        if signature != session.signature or secret is None:
            return
        token = object()
        session.wants_fetch = False
        session.fetch_token = token
        session.fetch_started = now
        # Debounce attempts, not successes: a failing endpoint is not retried sooner.
        session.next_fetch_at = now + DEBOUNCE_SECONDS
        self._fetch_in_flight = (token, now)
        job = (session.session_id, session.epoch, signature, token)
        try:
            thread = self._spawn_thread(self._run_fetch, job, secret)
            thread.start()
        except Exception:
            self._fetch_in_flight = None
            session.fetch_token = None
            self._record_failure(session, now)

    def _run_fetch(self, job, secret):
        try:
            try:
                windows = self._fetch_windows(*secret)
            except Exception:
                windows = None
            finally:
                del secret
            self._complete_fetch(job, windows)
        finally:
            # The slot is this thread's for as long as it lives, and only it gives it back.
            with self._lock:
                if self._fetch_in_flight is not None and self._fetch_in_flight[0] is job[3]:
                    self._fetch_in_flight = None
        self._flush_notify()

    def _complete_fetch(self, job, windows):
        session_id, epoch, signature, token = job
        with self._lock:
            session = self._sessions.get(session_id)
            if self._closed or session is None or session.fetch_token is not token:
                return  # abandoned, superseded, or the session is gone
            session.fetch_token = None
            now = self._now()
            # The answer is only as good as the route it was asked on. Look again.
            self._reconcile(session, self._find_agent(session_id))
            if session.epoch != epoch or session.signature != signature:
                return
            if not windows:
                self._record_failure(session, now)
                return
            session.failures = 0
            session.quota = {
                "epoch": epoch,
                "identity": signature.identity,
                "fetched_at": int(now),
                "windows": windows,
            }
            self._write(session)
            self._notify_pending = True

    def _record_failure(self, session, now):
        session.failures += 1
        backoff = min(DEBOUNCE_SECONDS * (2 ** session.failures), BACKOFF_MAX_SECONDS)
        session.next_fetch_at = max(session.next_fetch_at, now + backoff)

    def _expire_hung_fetch(self, now):
        if self._fetch_in_flight is None:
            return
        token, started = self._fetch_in_flight
        if now - started <= FETCH_DEADLINE_SECONDS:
            return
        # The thread cannot be killed. Its answer will no longer match any token, but it keeps
        # the single fetch slot until it really returns (`_run_fetch`): handing the slot back
        # here would start one more thread per attempt for as long as the call stays stuck.
        for session in self._sessions.values():
            if session.fetch_token is token:
                session.fetch_token = None
                self._record_failure(session, now)

    # ---------------------------------------------------------------- mailbox files

    def _acquire_lock(self, session_id):
        path = self._dir / (session_id + ".lock")
        try:
            fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600)
        except OSError:
            return None
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            # A previous owner unlinks its lock file on release. A lock taken on that
            # already-unlinked file would be invisible to the reader, so it does not count.
            if os.fstat(fd).st_ino != os.stat(path, follow_symlinks=False).st_ino:
                raise OSError("lock file was replaced")
        except OSError:
            os.close(fd)
            return None
        return fd

    def _write(self, session):
        if session.lock_fd is None:
            # A failed write gave the lock up. Without it this process may not be the owner.
            session.lock_fd = self._acquire_lock(session.session_id)
            if session.lock_fd is None:
                return False
        signature = session.signature
        record = {
            "schema": SCHEMA,
            "session_id": session.session_id,
            "pane_id": self._pane_id,
            "profile": "default",
            "pid": os.getpid(),
            "route": {
                "epoch": session.epoch,
                "provider": signature.provider,
                "supported": signature.supported,
                "identity": signature.identity,
                "observed_at": int(self._now()),
            },
            "quota": session.quota,
        }
        final = self._dir / (session.session_id + ".json")
        temporary = self._dir / ".{}.{}.tmp".format(session.session_id, os.getpid())
        try:
            try:
                os.unlink(temporary)
            except FileNotFoundError:
                pass
            fd = os.open(
                temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW | os.O_CLOEXEC, 0o600
            )
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                json.dump(record, handle, separators=(",", ":"))
            os.replace(temporary, final)
            session.written_at = self._now()
            return True
        except OSError:
            # The mailbox could not be rewritten, so an older quota may still be on disk.
            # Dropping the lock makes the reader treat this session as having no producer.
            self._drop_lock(session)
            return False

    def _drop_lock(self, session):
        if session.lock_fd is not None:
            try:
                os.close(session.lock_fd)
            except OSError:
                pass
            session.lock_fd = None

    def _release(self, session):
        session.fetch_token = None
        # Unlink while the lock is still held, so no other process can lock the old file.
        if session.lock_fd is not None:
            for suffix in (".json", ".lock"):
                try:
                    os.unlink(self._dir / (session.session_id + suffix))
                except OSError:
                    pass
        self._drop_lock(session)

    # ---------------------------------------------------------------- background work

    def _ensure_poller(self):
        with self._lock:
            if self._closed or not self._sessions:
                return
            if self._poller is not None and self._poller.is_alive():
                return
            try:
                self._poller = self._spawn_thread(self._poll_loop)
                self._poller.start()
            except Exception:
                self._poller = None

    def _poll_loop(self):
        while not self._stop.wait(self._poll_seconds):
            try:
                self.tick()
            except Exception:
                pass  # one bad pass must not end route tracking
            with self._lock:
                if not self._sessions:
                    self._poller = None
                    return

    def _flush_notify(self):
        with self._lock:
            now = self._now()
            if not self._notify_pending or self._notify is None or self._closed:
                return
            if now - self._last_notify < NOTIFY_MIN_INTERVAL_SECONDS:
                return  # still pending; the next tick sends it
            self._notify_pending = False
            self._last_notify = now
        try:
            self._notify()
        except Exception:
            pass


def _plain_thread(target, *args):
    return threading.Thread(target=target, args=args, name="herdr-agent-quota", daemon=True)


def _hermes_thread(target, *args):
    """A daemon thread that carries the spawner's Hermes context (profile home, secret scope)."""
    from agent.memory_provider import spawn_context_thread

    return spawn_context_thread(target, name="herdr-agent-quota", args=args)


class _Notifier:
    """Ask the quota plugin to republish this pane. Best effort; never blocks a caller."""

    def __init__(self, executable, state_dir):
        self._command = [executable, "hermes-notify"]
        self._state_dir = state_dir
        self._children = []

    def __call__(self):
        now = time.monotonic()
        alive = []
        for child, started in self._children:
            if child.poll() is None:
                if now - started > NOTIFY_CHILD_SECONDS:
                    child.kill()
                alive.append((child, started))
        self._children = alive
        if len(alive) >= 2:
            return
        environment = dict(os.environ)  # a copy: the process environment is never changed
        environment["HERDR_PLUGIN_STATE_DIR"] = self._state_dir
        child = subprocess.Popen(
            self._command, env=environment, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL, close_fds=True, start_new_session=True,
        )
        self._children.append((child, now))


def _owned_and_private(path, *, directory):
    """`path` is ours, is what it should be, and nobody else can write it. No symlinks."""
    try:
        info = os.lstat(path)
    except OSError:
        return False
    kind_ok = stat.S_ISDIR(info.st_mode) if directory else stat.S_ISREG(info.st_mode)
    return kind_ok and info.st_uid == os.getuid() and not info.st_mode & 0o022


def load_bridge_config(path):
    """`bridge.json`, written by `herdr-agent-quota configure`: where the mailbox lives."""
    if not _owned_and_private(path, directory=False):
        return None
    try:
        if os.path.getsize(path) > MAX_BRIDGE_CONFIG_BYTES:
            return None
        with open(path, encoding="utf-8") as handle:
            config = json.load(handle)
    except (OSError, ValueError):
        return None
    if not isinstance(config, dict):
        return None
    state_dir, executable = config.get("state_dir"), config.get("executable")
    for value in (state_dir, executable):
        if not isinstance(value, str) or not os.path.isabs(value) or "\0" in value:
            return None
    return state_dir, executable


def prepare_mailbox(state_dir):
    """The mailbox directory, private to this user, or None when that cannot be guaranteed."""
    if not _owned_and_private(state_dir, directory=True):
        return None
    mailbox = os.path.join(state_dir, MAILBOX_DIR)
    try:
        os.mkdir(mailbox, 0o700)
    except FileExistsError:
        pass
    except OSError:
        return None
    try:
        info = os.lstat(mailbox)
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
            return None
        if info.st_mode & 0o077:
            os.chmod(mailbox, 0o700)
    except OSError:
        return None
    return mailbox


def prune_mailbox(mailbox, now=None):
    """Remove what dead producers left behind (a killed process cannot clean up after itself).

    Bounded, and only ever a session whose lock nobody holds and that has been quiet for an hour.
    """
    now = time.time() if now is None else now
    try:
        names = sorted(os.listdir(mailbox))[:512]
    except OSError:
        return
    for name in names:
        stem, dot, suffix = name.rpartition(".")
        if not dot or suffix not in ("lock", "json", "tmp"):
            continue
        path = os.path.join(mailbox, name)
        try:
            if now - os.lstat(path).st_mtime < PRUNE_AFTER_SECONDS:
                continue
            if suffix == "lock":
                fd = os.open(path, os.O_RDWR | os.O_NOFOLLOW | os.O_CLOEXEC)
                try:
                    fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    for stale in (os.path.join(mailbox, stem + ".json"), path):
                        try:
                            os.unlink(stale)
                        except FileNotFoundError:
                            pass
                finally:
                    os.close(fd)
            elif suffix == "tmp" or not os.path.lexists(os.path.join(mailbox, stem + ".lock")):
                os.unlink(path)
        except OSError:
            continue  # held by a live producer, or already gone


def build_bridge(ctx, *, config_path=None, environ=None):
    """The bridge for this process, or None when any precondition is missing (inert)."""
    environ = os.environ if environ is None else environ
    pane_id = str(environ.get("HERDR_PANE_ID", "")).strip()
    if fcntl is None or environ.get("HERDR_ENV") != "1" or not _PANE_ID_RE.fullmatch(pane_id):
        return None
    # A named or relocated profile has its own credentials and its own session store; the
    # quota plugin reads the default profile's, so nothing may be published for another.
    if getattr(ctx, "profile_name", None) != "default":
        return None
    config = load_bridge_config(config_path or Path(__file__).with_name("bridge.json"))
    if config is None:
        return None
    state_dir, executable = config
    mailbox = prepare_mailbox(state_dir)
    if mailbox is None:
        return None
    prune_mailbox(mailbox)
    try:
        from agent.memory_provider import spawn_context_thread  # noqa: F401 - availability probe
    except Exception:
        return None  # without Hermes's context threads a worker could act for another scope
    return Bridge(
        mailbox, pane_id, notify=_Notifier(executable, state_dir), spawn_thread=_hermes_thread
    )


def register(ctx):
    bridge = build_bridge(ctx)
    if bridge is None:
        return

    def interactive(kwargs):
        session_id = kwargs.get("session_id")
        if kwargs.get("platform") in INTERACTIVE_PLATFORMS and isinstance(session_id, str):
            return session_id
        return None

    # Hook payloads carry the conversation; only the session id and platform are read.
    def observed(**kwargs):
        session_id = interactive(kwargs)
        if session_id:
            bridge.observe(session_id)

    def answered(**kwargs):
        session_id = interactive(kwargs)
        if session_id:
            bridge.observe(session_id, activity=True)

    def finalized(**kwargs):
        session_id = kwargs.get("session_id")
        if isinstance(session_id, str):
            bridge.finalize(session_id)

    for hook in ("on_session_start", "on_session_reset", "pre_llm_call", "pre_api_request"):
        ctx.register_hook(hook, observed)
    ctx.register_hook("post_api_request", answered)
    ctx.register_hook("on_session_finalize", finalized)
    on_unload = getattr(ctx, "on_unload", None)
    if callable(on_unload):
        on_unload(bridge.close)
    atexit.register(bridge.close)
