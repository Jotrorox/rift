"""Combined operational checks against real servers (Linux, including /proc)."""

from concurrent.futures import ThreadPoolExecutor
from contextlib import ExitStack
import http.client
import json
from pathlib import Path
import signal
import socket
import subprocess
import threading
import time
import uuid

import minecraft as mc


def eventually(predicate, description, timeout=40):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if value := predicate():
            return value
        time.sleep(0.05)
    raise AssertionError(f"timed out: {description}")


def metrics(port):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        connection.request("GET", "/metrics")
        response = connection.getresponse()
        assert response.status == 200, response.status
        return {key.removeprefix("rift_"): int(value)
                for line in response.read().decode().splitlines()
                if line and not line.startswith("#")
                for key, value in [line.rsplit(" ", 1)]}
    finally:
        connection.close()


def ports(count):
    # Reserve together so ephemeral port reuse cannot give two fixtures one port.
    with ExitStack() as stack:
        sockets = [stack.enter_context(socket.socket()) for _ in range(count)]
        for sock in sockets:
            sock.bind(("127.0.0.1", 0))
        return [sock.getsockname()[1] for sock in sockets]


def configuration(front, monitor, primary, lobby, target="primary", rate=10000,
                  capacity=64, shutdown_ms=45000):
    return f"""return {{
    listeners = {{ public = '127.0.0.1:{front}' }},
    backends = {{ primary = '127.0.0.1:{primary}', lobby = 'localhost:{lobby}' }},
    routes = {{ public = {{ ['play.test'] = '{target}', ['lobby.test'] = 'lobby',
                           ['primary.test'] = 'primary', ['*'] = '{target}' }} }},
    fallbacks = {{ primary = {{ 'lobby' }} }},
    limits = {{ max_connections = {capacity}, connect_timeout_ms = 3000, buffer_size = 32768 }},
    rate_limit = {{ per_ip_per_second = {rate}, per_ip_burst = {rate * 2},
                   global_per_second = {rate}, global_burst = {rate * 2}, max_ips = 16 }},
    health_check = {{ interval_ms = 500, timeout_ms = 250,
                      unhealthy_threshold = 2, healthy_threshold = 2 }},
    status_cache = {{ ttl_ms = 1000, max_entries = 16, max_response_bytes = 65536 }},
    metrics = '127.0.0.1:{monitor}',
    shutdown_timeout_ms = {shutdown_ms},
}}
"""


def write_config(path, source):
    temporary = path.with_suffix(".pending")
    temporary.write_text(source)
    temporary.replace(path)


def reload_config(proxy, monitor, path, source, valid=True):
    counter = "reloads_total" if valid else "reload_failures_total"
    before = metrics(monitor)
    write_config(path, source)
    proxy.send_signal(signal.SIGHUP)
    eventually(lambda: metrics(monitor)[counter] == before[counter] + 1, counter)
    other = "reload_failures_total" if valid else "reloads_total"
    assert metrics(monitor)[other] == before[other], "wrong reload outcome"


class Player:
    """One connection, no reconnects; background reads keep gameplay moving."""

    def __init__(self, front, name, protocol, compression, hostname):
        self.name = "RiftO" + uuid.uuid4().hex[:8]
        self.options = (front, self.name, protocol, compression, name == "pumpkin", hostname)
        self.lock = threading.Lock()
        self.client = None
        self.progress = {}
        self.error = None
        self.stopping = threading.Event()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def observe(self, client, progress):
        with self.lock:
            self.client, self.progress = client, progress

    def run(self):
        try:
            mc.play(*self.options, observe=self.observe)
        except (OSError, EOFError) as error:
            if not self.stopping.is_set():
                self.error = error
        except Exception as error:
            self.error = error

    def __enter__(self):
        self.thread.start()
        return self

    def snapshot(self):
        if self.error:
            raise AssertionError(f"{self.name} disconnected: {self.error}") from self.error
        assert self.thread.is_alive(), f"{self.name}: gameplay worker stopped"
        with self.lock:
            return self.progress.copy()

    def ready(self):
        eventually(lambda: (p := self.snapshot()).get("joined") and p.get("chunks", 0) > 0
                   and p.get("teleports", 0) > 0 and p.get("keepalives", 0) >= 2,
                   f"{self.name} login, chunks, teleport and two keepalives", timeout=80)

    def advance(self, field, previous):
        eventually(lambda: self.snapshot().get(field, 0) > previous,
                   f"{self.name}: fresh {field}")

    def disconnected(self):
        eventually(lambda: not self.thread.is_alive(), f"{self.name}: expected disconnect")
        assert self.error is not None, "expected the relay to close"

    def __exit__(self, *_):
        self.stopping.set()
        with self.lock:
            if self.client is not None:
                try:
                    self.client.socket.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        self.thread.join(timeout=25)
        assert not self.thread.is_alive(), "gameplay worker leaked"


def resource_sample(pid):
    root = Path(f"/proc/{pid}")
    status = dict(line.split(":", 1) for line in (root / "status").read_text().splitlines())
    descriptors = list((root / "fd").iterdir())
    sockets = 0
    for descriptor in descriptors:
        try:
            sockets += descriptor.readlink().as_posix().startswith("socket:")
        except FileNotFoundError:  # Short-lived status/health socket.
            pass
    return dict(rss_bytes=int(status["VmRSS"].split()[0]) * 1024,
                fds=len(descriptors), sockets=sockets)


def assert_resources(baseline, quiet, samples):
    # Fixed workload budgets, not a claim about arbitrary player capacity. Allow
    # allocator high-water retention, two health probes and an in-flight scrape.
    assert quiet, "no quiescent resource samples"
    for sample in quiet:
        assert sample["fds"] <= baseline["fds"] + 8, ("FD growth", baseline, sample)
        assert sample["sockets"] <= baseline["sockets"] + 6, ("socket growth", baseline, sample)
    for sample in samples + quiet:
        assert sample["rss_bytes"] <= baseline["rss_bytes"] + 16 * 1024 * 1024, (
            "RSS growth exceeds 16 MiB", baseline, sample)
        # 64 admitted connections, client + backend sockets, plus HTTP/probes.
        assert sample["fds"] <= baseline["fds"] + 2 * 64 + 20, ("peak FDs", sample)
        assert sample["sockets"] <= baseline["sockets"] + 2 * 64 + 20, ("peak sockets", sample)


class Resources:
    def __init__(self, proxy, directory):
        self.proxy, self.path = proxy, directory / "resources.jsonl"
        self.samples = []
        self.stop = threading.Event()
        self.error = None
        self.started = time.monotonic()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def sample(self):
        return dict(seconds=round(time.monotonic() - self.started, 3),
                    **resource_sample(self.proxy.pid))

    def run(self):
        try:
            with self.path.open("w") as output:
                while not self.stop.is_set():
                    try:
                        sample = self.sample()
                    except (OSError, KeyError) as error:
                        # /proc can briefly return EACCES while an exiting
                        # process loses its fd table, before waitpid sees it.
                        # Never hide an accounting failure on a live process.
                        try:
                            self.proxy.wait(timeout=0.5)
                        except subprocess.TimeoutExpired:
                            raise error
                        return
                    self.samples.append(sample)
                    output.write(json.dumps(sample) + "\n")
                    output.flush()
                    self.stop.wait(0.05)
        except Exception as error:
            self.error = error

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.thread.join(timeout=5)
        assert not self.thread.is_alive(), "resource sampler leaked"
        if self.error:
            raise self.error


def burst(front, protocol, expected, unique=False, count=128):
    def query(index):
        host = f"{uuid.uuid4().hex}.load.test" if unique else "play.test"
        reply = mc.status(front, protocol, host, ping=index - count // 2)
        assert reply["description"] == expected, (host, reply, expected)
        return reply
    with ThreadPoolExecutor(max_workers=16) as pool:
        list(pool.map(query, range(count)))


def rejected_status(front, protocol):
    try:
        mc.status(front, protocol)
    except (ConnectionResetError, BrokenPipeError, EOFError):
        return True
    return False


def teleport(server, player, index):
    previous = player.snapshot()["teleports"]
    server.stdin.write(f"tp {player.name} {32 * (index % 2)} 100 0\n")
    server.stdin.flush()
    player.advance("teleports", previous)


def stop_server(server):
    server.stdin.write("stop\n")
    server.stdin.flush()
    assert server.wait(timeout=40) == 0, "backend shutdown failed"


def listener_closed(front):
    try:
        with socket.create_connection(("127.0.0.1", front), timeout=0.2):
            return False
    except ConnectionRefusedError:
        return True
    except (ConnectionResetError, ConnectionAbortedError, TimeoutError):
        # A connect racing listener shutdown may be reset instead of refused.
        # Retry until a fresh connection is actually refused.
        return False


def test_operations(name, binary, directory, compression, rounds, result):
    directory.mkdir()
    protocol = mc.SERVERS[name]["protocol"]
    primary_port, lobby_port, front, monitor = ports(4)
    path = directory / "rift.lua"
    source = configuration(front, monitor, primary_port, lobby_port)
    write_config(path, source)
    quiet = result["quiet_resources"] = []
    result["rounds"] = rounds
    result["passed"] = False
    with ExitStack() as stack:
        servers, commands, directories = {}, {}, {}
        for role, port in [("primary", primary_port), ("lobby", lobby_port)]:
            server_dir = directory / role
            server_dir.mkdir()
            command = mc.configure_server(name, server_dir, port, compression,
                                          motd=f"rift-{name}-{role}-operations")
            servers[role] = stack.enter_context(mc.process(command, server_dir, "server.log", server=True))
            commands[role], directories[role] = command, server_dir
        for role, port in [("primary", primary_port), ("lobby", lobby_port)]:
            mc.wait_ready(servers[role], lambda p=port: mc.status_ready(p, protocol),
                          directories[role] / "server.log")
        descriptions = {role: mc.status(port, protocol)["description"]
                        for role, port in [("primary", primary_port), ("lobby", lobby_port)]}
        assert descriptions["primary"] != descriptions["lobby"]
        proxy = stack.enter_context(mc.process([str(binary), "--config", str(path)], directory, "proxy.log"))
        mc.wait_ready(proxy, lambda: "metrics on" in (directory / "proxy.log").read_text(), directory / "proxy.log")
        resources = stack.enter_context(Resources(proxy, directory))
        # Change this very hostname's route during the reload loop. Teleporting
        # the original player on primary proves that its relay did not migrate.
        primary = stack.enter_context(Player(front, name, protocol, compression, "play.test"))
        keeper = stack.enter_context(Player(front, name, protocol, compression, "lobby.test"))
        primary.ready()
        keeper.ready()
        before_reload_play = [primary.snapshot(), keeper.snapshot()]
        burst(front, protocol, descriptions["primary"], unique=True)
        eventually(lambda: metrics(monitor)["connections_active"] == 2, "warmup connections settle")
        baseline = result["baseline_resources"] = resources.sample()
        sample_start = len(resources.samples)
        print(f"{name}: operational gameplay established; starting {rounds} reload/status rounds", flush=True)
        for index in range(rounds):
            target = "lobby" if index % 2 == 0 else "primary"
            candidate = configuration(front, monitor, primary_port, lobby_port, target=target)
            before = metrics(monitor)
            reload_config(proxy, monitor, path, candidate)
            # The same handshake must see the new route immediately: no stale
            # cache generation. Compare MOTDs, since live player counts vary.
            assert mc.status(front, protocol, "play.test")["description"] == descriptions[target]
            assert metrics(monitor)["status_cache_misses_total"] > before["status_cache_misses_total"]
            burst(front, protocol, descriptions[target])
            burst(front, protocol, descriptions[target], unique=True)
            after = metrics(monitor)
            assert after["status_cache_hits_total"] > before["status_cache_hits_total"]
            if index % 3 == 0:
                invalid = ["return {", candidate.replace("max_connections = 64", "max_connections = 0"),
                           candidate.replace(f"public = '127.0.0.1:{front}'", "public = '127.0.0.1:0'"),
                           candidate.replace(f"metrics = '127.0.0.1:{monitor}'", "metrics = '127.0.0.1:0'")]
                reload_config(proxy, monitor, path, invalid[(index // 3) % len(invalid)], valid=False)
                assert mc.status(front, protocol, "play.test")["description"] == descriptions[target]
            # Console teleports require the original sessions to receive and
            # acknowledge new play packets after every accepted/rejected reload.
            teleport(servers["primary"], primary, index)
            teleport(servers["lobby"], keeper, index)
            eventually(lambda: metrics(monitor)["connections_active"] == 2, "burst connections settle")
            quiet.append(resources.sample())
            assert_resources(baseline, quiet, resources.samples[sample_start:])
        for player, previous in zip([primary, keeper], before_reload_play):
            player.advance("keepalives", previous["keepalives"])
        result["reload_gameplay"] = [primary.snapshot(), keeper.snapshot()]

        # Existing players survive both kinds of admission rejection.
        reload_config(proxy, monitor, path, configuration(front, monitor, primary_port, lobby_port, capacity=1))
        before = metrics(monitor)["connections_capacity_rejected_total"]
        assert all(rejected_status(front, protocol) for _ in range(16))
        assert metrics(monitor)["connections_capacity_rejected_total"] == before + 16
        teleport(servers["lobby"], keeper, 0)
        reload_config(proxy, monitor, path, configuration(front, monitor, primary_port, lobby_port, rate=1))
        before = metrics(monitor)["connections_rate_limited_total"]
        with ThreadPoolExecutor(max_workers=16) as pool:
            rejected = sum(pool.map(lambda _: rejected_status(front, protocol), range(32)))
        assert rejected > 0, "rate limiter never rejected the burst"
        assert metrics(monitor)["connections_rate_limited_total"] == before + rejected
        teleport(servers["primary"], primary, 0)
        teleport(servers["lobby"], keeper, 1)
        reload_config(proxy, monitor, path, source)

        # Stop a real backend. Its own player must disconnect; the lobby player
        # must remain on the same socket. New players should reach the fallback.
        previous = keeper.snapshot()["keepalives"]
        stop_server(servers["primary"])
        primary.disconnected()
        eventually(lambda: metrics(monitor)['backend_up{backend="primary"}'] == 0, "primary marked down")
        time.sleep(1.1)  # Let the 1 s status TTL expire; stale status cannot hide outage.
        burst(front, protocol, descriptions["lobby"])
        fallback = stack.enter_context(Player(front, name, protocol, compression, "primary.test"))
        fallback.ready()
        keeper.advance("keepalives", previous)
        assert metrics(monitor)["fallbacks_total"] >= 2, "status and login did not select fallback"
        result["outage_gameplay"] = keeper.snapshot()
        recovered = stack.enter_context(mc.process(commands["primary"], directories["primary"], "restarted.log", server=True))
        mc.wait_ready(recovered, lambda: mc.status_ready(primary_port, protocol), directories["primary"] / "restarted.log")
        eventually(lambda: metrics(monitor)['backend_up{backend="primary"}'] == 1, "primary recovered")
        time.sleep(1.1)
        assert mc.status(front, protocol, "play.test")["description"] == descriptions["primary"]
        previous = fallback.snapshot()["keepalives"]
        mc.play(front, "RiftN" + uuid.uuid4().hex[:8], protocol, compression, name == "pumpkin", "primary.test")
        fallback.advance("keepalives", previous)
        teleport(servers["lobby"], fallback, 0)  # Recovery must not migrate this player.
        eventually(lambda: metrics(monitor)["connections_active"] == 2, "recovery connections settle")
        quiet.append(resources.sample())
        assert_resources(baseline, quiet, resources.samples[sample_start:])
        result["metrics_before_shutdown"] = metrics(monitor)
        assert result["metrics_before_shutdown"]["health_check_failures_total"] > 0
        assert result["metrics_before_shutdown"]["health_checks_total"] > 0

        # Drain with both players exchanging packets; metrics remains available.
        previous = [keeper.snapshot()["keepalives"], fallback.snapshot()["keepalives"]]
        proxy.send_signal(signal.SIGTERM)
        eventually(lambda: listener_closed(front), "listener closes during drain")
        assert proxy.poll() is None
        eventually(lambda: metrics(monitor)["connections_active"] == 2, "draining players remain")
        draining = metrics(monitor)
        assert draining["connections_active"] == 2
        for player, keepalives in zip([keeper, fallback], previous):
            player.advance("keepalives", keepalives)
            teleport(servers["lobby"], player, 1)
        after = metrics(monitor)
        assert after["client_bytes_read_total"] > draining["client_bytes_read_total"]
        assert after["client_bytes_written_total"] > draining["client_bytes_written_total"]
        assert after["forced_shutdowns_total"] == 0
        assert after["health_checks_total"] == draining["health_checks_total"], "health probes continued draining"
        result["drain_gameplay"] = [keeper.snapshot(), fallback.snapshot()]
        keeper.__exit__()
        fallback.__exit__()
        assert proxy.wait(timeout=10) == 0, "graceful shutdown failed"
        assert "shutdown deadline reached" not in (directory / "proxy.log").read_text()
        assert_resources(baseline, quiet, resources.samples[sample_start:])
        result["peak_resources"] = {key: max(s[key] for s in resources.samples[sample_start:])
                                    for key in ["rss_bytes", "fds", "sockets"]}
        result["resource_samples"] = len(resources.samples)

        # Both forced shutdown paths with an actual logged-in player, not a
        # synthetic idle socket. Use separate proxies so counters are unambiguous.
        for second_signal in [False, True]:
            label = "second-signal" if second_signal else "deadline"
            write_config(path, configuration(front, monitor, primary_port, lobby_port,
                                             shutdown_ms=45000 if second_signal else 1500))
            with mc.process([str(binary), "--config", str(path)], directory, f"{label}.log") as forced:
                mc.wait_ready(forced, lambda: "metrics on" in (directory / f"{label}.log").read_text(),
                              directory / f"{label}.log")
                with Player(front, name, protocol, compression, "lobby.test") as player:
                    player.ready()
                    started = time.monotonic()
                    forced.send_signal(signal.SIGTERM)
                    eventually(lambda: listener_closed(front), "forced drain starts")
                    eventually(lambda: metrics(monitor)["connections_active"] == 1, "forced drain player remains")
                    teleport(servers["lobby"], player, 0)
                    if second_signal:
                        forced.send_signal(signal.SIGINT)
                    assert forced.wait(timeout=6) == 0
                    player.disconnected()
                    elapsed = time.monotonic() - started
                    if not second_signal:
                        assert elapsed >= 1.4, "shutdown ignored configured deadline"
                    expected = "second shutdown signal" if second_signal else "shutdown deadline reached"
                    assert expected in (directory / f"{label}.log").read_text()
                    result[label] = dict(seconds=round(elapsed, 3), passed=True)
    assert servers["lobby"].returncode == recovered.returncode == 0, "backend cleanup failed"
    result["passed"] = True
    print(f"PASS {name}: operational reloads, outages, cache/admission bursts, resources and gameplay during shutdown", flush=True)
