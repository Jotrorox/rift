"""Machine-wide resources for the translation matrix: caches, locks, ports,
JDKs and the processes it starts.

Many developers run the matrix at once on one machine, so everything that is
expensive or exclusive is guarded across processes with `fcntl.flock`:
downloads and builds take a per-artifact lock, at most MAX_SERVERS Minecraft
servers run at a time (one slot file each), and ports are reserved with a lock
file per port on top of a bind test. Locks die with their process, so a crash
never leaves a stale lock behind.

Every child runs in its own process group with a watchdog member that kills
the group if this process disappears without cleaning up (SIGKILL, a closed
terminal); normal exits, failures, timeouts and Ctrl-C stop groups directly.
"""

from contextlib import contextmanager
import atexit
import errno
import fcntl
import hashlib
import os
from pathlib import Path
import random
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request

from minecraft import java_binary as java  # noqa: F401  (RIFT_JAVA_<n>, then HOME/jdks)

HOME = Path(os.environ.get("RIFT_VIA_CACHE") or Path.home() / ".cache" / "rift-via")
LOCKS = HOME / "locks"
MAX_SERVERS = int(os.environ.get("RIFT_VIA_MAX_SERVERS", "3"))
# Below macOS's and Linux's ephemeral ranges, so outgoing connections never
# hold a port the matrix hands out.
PORTS = range(21000, 32000)
USER_AGENT = "rift-translation-matrix/0.1.0 (https://github.com/Jotrorox/rift)"


def log(message):
    print(message, file=sys.stderr, flush=True)


class Lock:
    """An exclusive flock on LOCKS/<name>, held until release()."""

    def __init__(self, name):
        self.path = LOCKS / name
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.fd = None

    def try_acquire(self):
        fd = os.open(self.path, os.O_RDWR | os.O_CREAT, 0o644)
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except OSError as error:
            os.close(fd)
            if error.errno in (errno.EAGAIN, errno.EACCES, errno.EWOULDBLOCK):
                return False
            raise
        self.fd = fd
        os.ftruncate(fd, 0)
        os.write(fd, f"{os.getpid()}\n".encode())
        return True

    def owner(self):
        try:
            return self.path.read_text().strip() or "?"
        except OSError:
            return "?"

    def release(self):
        if self.fd is not None:
            os.close(self.fd)
            self.fd = None


@contextmanager
def locked(name, what, timeout=3600):
    """Hold LOCKS/<name>; say who we wait for when another process has it."""
    lock = Lock(name)
    deadline = time.monotonic() + timeout
    announced = False
    while not lock.try_acquire():
        if not announced:
            log(f"waiting for {what} (lock {lock.path.name} held by pid {lock.owner()})")
            announced = True
        if time.monotonic() > deadline:
            raise TimeoutError(f"gave up waiting for {what} after {timeout}s")
        time.sleep(0.5)
    try:
        yield
    finally:
        lock.release()


def acquire_server_slot(label, timeout=None):
    """One of MAX_SERVERS machine-wide server slots; release() it when the
    server has stopped."""
    timeout = float(os.environ.get("RIFT_VIA_SLOT_TIMEOUT", "5400")) if timeout is None else timeout
    deadline = time.monotonic() + timeout
    announced = False
    while True:
        for index in range(MAX_SERVERS):
            lock = Lock(f"server-slot-{index}")
            if lock.try_acquire():
                os.write(lock.fd, f"{label}\n".encode())
                return lock
        if not announced:
            owners = [Lock(f"server-slot-{index}").owner().replace("\n", " ")
                      for index in range(MAX_SERVERS)]
            log(f"waiting for one of {MAX_SERVERS} machine-wide Minecraft server slots "
                f"(held by: {'; '.join(owners)})")
            announced = True
        if time.monotonic() > deadline:
            raise TimeoutError(f"no server slot became free within {timeout}s")
        time.sleep(1)


_ports = {}


def allocate_port():
    """A loopback port nobody listens on and no other matrix process holds."""
    for _ in range(500):
        port = random.choice(PORTS)
        if port in _ports:
            continue
        lock = Lock(f"ports/{port}")
        if not lock.try_acquire():
            continue
        with socket.socket() as probe:
            try:
                probe.bind(("127.0.0.1", port))
            except OSError:
                lock.release()
                continue
        _ports[port] = lock
        return port
    raise RuntimeError("no free port found")


def release_port(port):
    lock = _ports.pop(port, None)
    if lock:
        lock.release()


def sha(path, algorithm):
    with open(path, "rb") as source:
        return hashlib.file_digest(source, algorithm).hexdigest()


def fetch(url, destination, algorithm, checksum, what=None):
    """Download `url` to `destination` once, verified, under a per-file lock."""
    destination = Path(destination)
    if destination.exists() and sha(destination, algorithm) == checksum:
        return destination
    with locked(f"download-{destination.name}", f"download of {destination.name}"):
        if destination.exists():
            actual = sha(destination, algorithm)
            if actual == checksum:
                return destination
            raise ValueError(f"checksum mismatch in cache: {destination}: {actual}")
        destination.parent.mkdir(parents=True, exist_ok=True)
        log(f"downloading {what or destination.name} from {url}")
        request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        with tempfile.NamedTemporaryFile(dir=destination.parent, delete=False) as pending:
            try:
                with urllib.request.urlopen(request, timeout=300) as response:
                    while block := response.read(1 << 20):
                        pending.write(block)
                pending.close()
                actual = sha(pending.name, algorithm)
                if actual != checksum:
                    raise ValueError(f"checksum mismatch: {url}: {actual}")
                Path(pending.name).replace(destination)
            finally:
                Path(pending.name).unlink(missing_ok=True)
    return destination


# Kills the process group once the harness that started it is gone. It is a
# member of the group, so stopping the group normally ends it too.
WATCHDOG = """
import os, signal, sys, time
parent, group = int(sys.argv[1]), int(sys.argv[2])
while os.getppid() == parent:
    time.sleep(0.5)
    try:
        os.killpg(group, 0)
    except OSError:
        sys.exit(0)
os.killpg(group, signal.SIGKILL)
"""


class Processes:
    """Children of this harness, each in its own process group."""

    def __init__(self):
        self.groups = {}
        self.lock = threading.Lock()

    def spawn(self, command, cwd, log_path, env=None, stdin=True):
        log_file = open(log_path, "ab")
        proc = subprocess.Popen(
            command, cwd=cwd, env=env, stdout=log_file, stderr=subprocess.STDOUT,
            stdin=subprocess.PIPE if stdin else subprocess.DEVNULL, process_group=0)
        log_file.close()
        watchdog = subprocess.Popen(
            [sys.executable, "-I", "-c", WATCHDOG, str(os.getpid()), str(proc.pid)],
            process_group=proc.pid, stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        with self.lock:
            self.groups[proc.pid] = (proc, watchdog)
        return proc

    def stop(self, proc, grace=10, command=None):
        """Stop a child's whole group: `command` on stdin first if given (a
        server's `stop`), then SIGTERM, then SIGKILL."""
        with self.lock:
            entry = self.groups.pop(proc.pid, None)
        if proc.poll() is None and command and proc.stdin:
            try:
                proc.stdin.write(command.encode() + b"\n")
                proc.stdin.flush()
                proc.wait(timeout=grace)
            except (BrokenPipeError, OSError, subprocess.TimeoutExpired):
                pass
        watchdog = entry[1] if entry else None
        for sig, wait in ((signal.SIGTERM, grace), (signal.SIGKILL, 10)):
            try:
                os.killpg(proc.pid, sig)
            except ProcessLookupError:
                break
            try:
                proc.wait(timeout=wait)
                if watchdog:
                    watchdog.wait(timeout=2)  # Reap it: zombies still count as members.
            except subprocess.TimeoutExpired:
                continue
            try:
                # The group may outlive its leader (forks).
                os.killpg(proc.pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.2)
        if proc.stdin:
            try:
                proc.stdin.close()
            except OSError:
                pass
        if watchdog and watchdog.poll() is None:
            watchdog.kill()
            watchdog.wait()

    def stop_all(self):
        for proc, _ in list(self.groups.values()):
            self.stop(proc, grace=3)


PROCESSES = Processes()
atexit.register(PROCESSES.stop_all)


def _terminate(signum, frame):
    # Turn termination into an exception so `finally` blocks run in order.
    signal.signal(signum, signal.SIG_DFL)
    raise KeyboardInterrupt(f"signal {signum}")


def install_signal_handlers():
    for sig in (signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, _terminate)
