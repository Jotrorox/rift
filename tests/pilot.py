#!/usr/bin/env python3
"""Repeatable local release pilot: buffer sweep, live reload, admission and drain (Unix)."""

import argparse
from contextlib import contextmanager, ExitStack
from datetime import datetime, timezone
import hashlib
import http.client
import json
import math
import os
from pathlib import Path
import platform
import signal
import socketserver
import statistics
import subprocess
import tempfile
import threading
import time

from bench import MIB, connect, latency, throughput, socket_frame, packet, login_name, login_success
from minecraft import ROOT, process
from prometheus import parse_metrics


@contextmanager
def backend(uppercase=False):
    class Handler(socketserver.BaseRequestHandler):
        def handle(self):
            self.request.settimeout(15)
            try:
                with self.request.makefile("rb") as reader:
                    socket_frame(reader)  # Handshake.
                    name = login_name(socket_frame(reader))
                    self.request.sendall(login_success(name))
                    while True:
                        data = socket_frame(reader)
                        self.request.sendall(packet(data[:1] + (data[1:].upper() if uppercase else data[1:])))
            except (EOFError, OSError, ValueError):
                pass

    class Server(socketserver.ThreadingTCPServer):
        daemon_threads = True

    with Server(("127.0.0.1", 0), Handler) as server:
        worker = threading.Thread(target=server.serve_forever)
        worker.start()
        try:
            yield server.server_address[1]
        finally:
            server.shutdown()
            worker.join()


def wait_for(proxy, predicate, log, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if proxy.poll() is not None:
            raise RuntimeError(f"Rift exited early: {log.read_text()}")
        if predicate():
            return
        time.sleep(0.01)
    raise TimeoutError(f"pilot deadline: {log.read_text()}")


def config(port, buffer_size=None, max_connections=None, shutdown_ms=None):
    limits = []
    if buffer_size is not None:
        limits.append(f"buffer_size = {buffer_size}")
    if max_connections is not None:
        limits.append(f"max_connections = {max_connections}")
    shutdown = f"shutdown_timeout_ms = {shutdown_ms}," if shutdown_ms else ""
    return f"""return {{
    listeners = {{ public = '127.0.0.1:0' }},
    backends = {{ main = '127.0.0.1:{port}' }},
    routes = {{ public = 'main' }},
    limits = {{ {', '.join(limits)} }},
    metrics = '127.0.0.1:0',
    {shutdown}
}}
"""


def replace(path, source):
    candidate = path.with_suffix(".next")
    candidate.write_text(source, encoding="utf-8")
    candidate.replace(path)


@contextmanager
def running(binary, directory, source):
    path = directory / "rift.lua"
    replace(path, source)
    log = directory / "proxy.log"
    with process([str(binary), "--config", str(path)], directory, log.name) as proxy:
        wait_for(proxy, lambda: "rift: metrics on " in log.read_text(), log)
        lines = log.read_text().splitlines()
        ports = [int(next(line for line in lines if line.startswith(prefix)).rsplit(":", 1)[1])
                 for prefix in ["rift: listening on ", "rift: metrics on "]]
        yield proxy, path, log, *ports


def scrape(port):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=3)
    try:
        connection.request("GET", "/metrics")
        response = connection.getresponse()
        assert response.status == 200
        return parse_metrics(response.read().decode())
    finally:
        connection.close()


def exchange(client, expected=b"pilot"):
    client.sendall(b"pilot")
    data = b""
    while len(data) < len(expected):
        part = client.recv(len(expected) - len(data))
        assert part, "premature EOF"
        data += part
    assert data == expected, data


def rss_kib(pid):
    status = Path(f"/proc/{pid}/status")
    if not status.exists():
        return None
    return int(next(line.split()[1] for line in status.read_text().splitlines()
                    if line.startswith("VmRSS:")))


def sweep(binary, directory, port):
    results = []
    for size in [8192, 16384, 32768, 65536]:
        with running(binary, directory, config(port, buffer_size=size)) as run:
            proxy, _, _, frontend, _ = run
            p50, p95 = latency(frontend)
            single = [throughput(frontend, 1, 64 * MIB) for _ in range(3)]
            parallel = [throughput(frontend, 16, 16 * MIB) for _ in range(3)]
            with ExitStack() as clients:
                for _ in range(16):
                    exchange(clients.enter_context(connect(frontend)))
                resident = rss_kib(proxy.pid)
            row = dict(buffer_size=size, rtt_p50_us=p50, rtt_p95_us=p95,
                       single_mib_s=statistics.median(single),
                       parallel_mib_s=statistics.median(parallel),
                       single_samples=single, parallel_samples=parallel,
                       rss_with_16_sessions_kib=resident)
            results.append(row)
            print(json.dumps(row), flush=True)
    return results


def operations(binary, directory, first, second, seconds):
    # Small explicit cap makes admission behavior observable without a load test.
    source = config(first, max_connections=16)
    with running(binary, directory, source) as run, ExitStack() as stack:
        proxy, path, log, frontend, metrics = run
        clients = [stack.enter_context(connect(frontend)) for _ in range(16)]
        for client in clients:
            exchange(client)
        with connect(frontend) as overflow:
            assert overflow.recv(1) == b"", "connection cap not enforced"
        wait_for(proxy, lambda: scrape(metrics)["rift_connections_capacity_rejected_total"] == 1, log)

        replace(path, "return { broken = true }")
        proxy.send_signal(signal.SIGHUP)
        wait_for(proxy, lambda: scrape(metrics)["rift_reload_failures_total"] == 1, log)
        for client in clients:
            exchange(client)
        # Free one slot, then switch backend. Existing sessions must stay on first.
        clients.pop().close()
        wait_for(proxy, lambda: scrape(metrics)["rift_connections_active"] == 15, log)
        replace(path, config(second, max_connections=16))
        proxy.send_signal(signal.SIGHUP)
        wait_for(proxy, lambda: scrape(metrics)["rift_reloads_total"] == 1, log)
        new = stack.enter_context(connect(frontend))
        exchange(new, b"PILOT")
        rounds = 0
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            for client in clients:
                exchange(client)
            exchange(new, b"PILOT")
            rounds += 1
            time.sleep(0.01)
        counters = scrape(metrics)
        assert counters["rift_connections_active"] == 16
        assert counters["rift_connection_errors_total"] == 0
        assert counters["rift_backend_connect_failures_total"] == 0
        start = time.monotonic()
        proxy.send_signal(signal.SIGTERM)
        wait_for(proxy, lambda: "rift: draining 16 connections" in log.read_text(), log)
        # Traffic survives the start of drain, including both backend generations.
        for client in clients:
            exchange(client)
        exchange(new, b"PILOT")
        assert scrape(metrics)["rift_connections_active"] == 16
        stack.close()
        assert proxy.wait(timeout=5) == 0
        assert "shutdown deadline reached" not in log.read_text()
        assert "rift: shutdown complete" in log.read_text()
        return dict(sessions=16, soak_seconds=seconds, rounds=rounds,
                    successful_soak_exchanges=rounds * 16,
                    invalid_reload_kept_sessions=True, new_route_verified=True,
                    drain_seconds=time.monotonic() - start, metrics=counters)


def forced_drain(binary, directory, port):
    with running(binary, directory, config(port, shutdown_ms=250)) as run:
        proxy, _, log, frontend, _ = run
        with connect(frontend) as client:
            exchange(client)
            start = time.monotonic()
            proxy.send_signal(signal.SIGTERM)
            assert proxy.wait(timeout=5) == 0
            assert client.recv(1) == b""
            assert "shutdown deadline reached" in log.read_text()
            return dict(configured_timeout_ms=250, elapsed_seconds=time.monotonic() - start)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/rift")
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--seconds", type=float, default=10)
    args = parser.parse_args()
    if os.name != "posix" or not __debug__ or not math.isfinite(args.seconds) or args.seconds <= 0:
        parser.error("requires Unix, assertions enabled (no -O), and --seconds > 0")
    binary = args.binary.resolve(strict=True)
    report = dict(timestamp=datetime.now(timezone.utc).isoformat(),
                  platform=platform.platform(), cpu_count=os.cpu_count(),
                  python=platform.python_version(),
                  version=subprocess.check_output([str(binary), "--version"], text=True).strip(),
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest())
    with tempfile.TemporaryDirectory(prefix="rift-pilot-") as temporary, backend() as first, backend(True) as second:
        directory = Path(temporary)
        report["buffers"] = sweep(binary, directory, first)
        report["operations"] = operations(binary, directory, first, second, args.seconds)
        report["forced_drain"] = forced_drain(binary, directory, first)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(f"Pilot passed: {args.report}")


if __name__ == "__main__":
    main()
