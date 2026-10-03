"""Harmless isolation probe. Runs with the system Python; executes no Hermes code."""
import errno
import os
import socket
import sys

H = "/home/hadas"
ok = True


def check(label, got, want):
    global ok
    good = got == want
    ok = ok and good
    print(f"{'PASS' if good else 'FAIL'} {label}: {got}")


def write_attempt(path):
    try:
        fd = os.open(os.path.join(path, ".probe-write"), os.O_CREAT | os.O_WRONLY, 0o600)
    except OSError as exc:
        return errno.errorcode.get(exc.errno, str(exc.errno))
    os.close(fd)
    os.unlink(os.path.join(path, ".probe-write"))
    return "WRITABLE"


def open_rw(path):
    try:
        os.close(os.open(path, os.O_RDWR))
    except OSError as exc:
        return errno.errorcode.get(exc.errno, str(exc.errno))
    return "WRITABLE"


print("uid_map:", open("/proc/self/uid_map").read().split())
print("pid:", os.getpid(), "env keys:", sorted(os.environ))
print("net ifaces:", sorted(os.listdir("/sys/class/net")) if os.path.isdir("/sys/class/net") else "no /sys")
print("home entries:", sorted(os.listdir(H)))
print(".hermes entries:", sorted(os.listdir(f"{H}/.hermes")))

for ro in ("/usr", f"{H}/.hermes/hermes-agent", f"{H}/.hermes/hermes-agent/.hermes/bin",
           f"{H}/.hermes/tools", f"{H}/.hermes/installs", "/plugin-src"):
    check(f"read-only {ro}", write_attempt(ro), "EROFS")
check("launcher not writable", open_rw(f"{H}/.hermes/hermes-agent/.hermes/bin/hermes"), "EROFS")
for rw in ("/work", f"{H}/.hermes"):
    check(f"writable {rw}", write_attempt(rw), "WRITABLE")

for absent in (f"{H}/.hermes/auth.json", f"{H}/.hermes/config.yaml", f"{H}/.hermes/.env",
               f"{H}/.hermes/state.db", f"{H}/.hermes/sessions", f"{H}/.hermes/plugins",
               f"{H}/.config", f"{H}/.config/herdr/herdr.sock", f"{H}/.ssh", f"{H}/projects",
               "/run/user/1000", "/etc/passwd", "/mnt/c"):
    check(f"absent {absent}", os.path.lexists(absent), False)

try:
    socket.create_connection(("1.1.1.1", 53), timeout=2).close()
    net = "CONNECTED"
except OSError as exc:
    net = errno.errorcode.get(exc.errno, repr(exc))
check("network blocked", net, "ENETUNREACH")

print("PROBE", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
