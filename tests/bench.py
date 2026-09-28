#!/usr/bin/env python3
"""Small loopback benchmark; Python echo/client overhead is included in both routes."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
import json
import platform
import socket
import statistics
import subprocess
import tempfile
import threading
import time
from pathlib import Path

from minecraft import ROOT, process, unused_port, wait_ready

BLOCK = b"x" * 65536
MIB = 1024 * 1024


def connect(port):
    sock = socket.create_connection(("127.0.0.1", port), timeout=30)
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return sock


def echo(client):
    with client:
        client.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        while data := client.recv(65536):
            client.sendall(data)


def serve(listener, stop):
    listener.settimeout(0.2)
    with ThreadPoolExecutor(max_workers=32) as pool:
        while not stop.is_set():
            try:
                client, _ = listener.accept()
            except socket.timeout:
                continue
            pool.submit(echo, client)


def latency(port):
    samples = []
    with connect(port) as sock:
        for index in range(1100):
            start = time.perf_counter_ns()
            sock.sendall(b"ping")
            received = b""
            while len(received) < 4:
                part = sock.recv(4 - len(received))
                assert part, "premature EOF"
                received += part
            assert received == b"ping"
            if index >= 100:
                samples.append((time.perf_counter_ns() - start) / 1000)
    return statistics.median(samples), sorted(samples)[949]


def transfer(port, size):
    with connect(port) as sock:
        def send():
            for _ in range(size // len(BLOCK)):
                sock.sendall(BLOCK)
            sock.shutdown(socket.SHUT_WR)

        with ThreadPoolExecutor(max_workers=1) as pool:
            sender = pool.submit(send)
            received = 0
            while data := sock.recv(65536):
                received += len(data)
            sender.result()
        assert received == size, (received, size)


def throughput(port, clients, size):
    start = time.perf_counter()
    with ThreadPoolExecutor(max_workers=clients) as pool:
        list(pool.map(lambda _: transfer(port, size), range(clients)))
    # Count payload once, although echo transfers it in both directions.
    return clients * size / MIB / (time.perf_counter() - start)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path, help="write machine-readable results for comparisons")
    args = parser.parse_args()
    if not __debug__:
        parser.error("do not use python -O: transfer assertions must be enabled")
    report = {"platform": platform.platform(), "routes": {}}
    subprocess.run(["cargo", "build", "--release", "--locked"], cwd=ROOT, check=True)
    with closing(socket.socket()) as listener, tempfile.TemporaryDirectory(prefix="rift-bench-") as temp:
        listener.bind(("127.0.0.1", 0))
        listener.listen(128)
        backend = listener.getsockname()[1]
        frontend = unused_port()
        stop = threading.Event()
        server = threading.Thread(target=serve, args=(listener, stop))
        server.start()
        try:
            directory = Path(temp)
            log = directory / "proxy.log"
            with process([str(ROOT / "target/release/rift"), f"127.0.0.1:{frontend}",
                          f"127.0.0.1:{backend}"], directory, "proxy.log") as proxy:
                wait_ready(proxy, lambda: "rift:" in log.read_text(), log)
                print("route     RTT p50/p95 (us)   1 client (MiB/s)   16 clients (MiB/s)")
                for name, port in [("direct", backend), ("rift", frontend)]:
                    p50, p95 = latency(port)
                    single = statistics.median(throughput(port, 1, 64 * MIB) for _ in range(3))
                    parallel = statistics.median(throughput(port, 16, 16 * MIB) for _ in range(3))
                    report["routes"][name] = dict(rtt_p50_us=p50, rtt_p95_us=p95,
                                                 single_mib_s=single, parallel_mib_s=parallel)
                    print(f"{name:8}  {p50:6.1f}/{p95:<6.1f}      {single:8.1f}             {parallel:8.1f}", flush=True)
        finally:
            stop.set()
            server.join()
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")


if __name__ == "__main__":
    main()
