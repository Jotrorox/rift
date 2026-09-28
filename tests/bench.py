#!/usr/bin/env python3
"""Loopback relay throughput and connection bursts, including Lua and cached status.

Uses only the Python standard library. CPU/RSS collection requires Linux /proc;
other platforms still run the traffic benchmark and report null resource values.
"""

import argparse
import asyncio
from collections import Counter
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import http.client
import io
import json
import math
import os
from pathlib import Path
import platform
import re
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid

from minecraft import ROOT, process, string, varint, read_varint, wait_ready

BLOCK = b"x" * 65536
MIB = 1024 * 1024
HOSTNAME = "bench.example.test"
PROBE = b"rift-benchmark-ready"
STATUS_JSON = json.dumps({
    "version": {"name": "benchmark", "protocol": 774},
    "players": {"max": 100, "online": 0},
    "description": {"text": "rift benchmark"},
}, separators=(",", ":"))
STATUS_BODY = b"\0" + string(STATUS_JSON)
STATUS_REPLY = varint(len(STATUS_BODY)) + STATUS_BODY
SCENARIOS = ("direct", "rift", "hostname", "lua", "lua_init", "status_cached")
SAMPLE_INTERVAL = 0.005


def handshake(port, state=2):
    body = b"\0" + varint(47) + string(HOSTNAME) + struct.pack(">H", port) + varint(state)
    return varint(len(body)) + body


def packet(body):
    return varint(len(body)) + body


_identity_lock = threading.Lock()
_next_identity = 0


def new_player_name():
    """Unique across threaded transfers, burst waves, warmups and scenarios."""
    global _next_identity
    with _identity_lock:
        _next_identity += 1
        identity = _next_identity
    if identity >= 1 << 32:
        raise OverflowError("benchmark player names exhausted")
    # Nine characters, just like Benchmark: packet lengths stay comparable.
    return f"B{identity:08x}"


def login_start(name):
    return packet(b"\0" + string(name))


def login_name(body):
    reader = io.BytesIO(body)
    if read_varint(reader) != 0:
        raise ValueError("invalid login start")
    length = read_varint(reader)
    if not 1 <= length <= 16:
        raise ValueError("invalid login name length")
    name = reader.read(length)
    if len(name) != length or not re.fullmatch(rb"[A-Za-z0-9_]+", name) or reader.read(1):
        raise ValueError("invalid login name")
    return name.decode("ascii")


def login_success(name):
    digest = hashlib.md5(f"OfflinePlayer:{name}".encode(), usedforsecurity=False).digest()
    identity = uuid.UUID(bytes=digest, version=3)
    return packet(b"\x02" + string(str(identity)) + string(name))


def socket_frame(reader):
    length = read_varint(reader)
    if not 0 < length <= 2 * MIB:
        raise ValueError("invalid frame length")
    body = reader.read(length)
    if len(body) != length:
        raise EOFError("truncated frame")
    return body


class GameSocket:
    """Expose fixture payload bytes while carrying proper Minecraft play packets."""
    def __init__(self, sock, port):
        self.socket = sock
        self.reader = sock.makefile("rb")
        self.pending = bytearray()
        self.closed = False
        self.name = new_player_name()
        try:
            sock.sendall(handshake(port) + login_start(self.name))
            if packet(socket_frame(self.reader)) != login_success(self.name):
                raise ValueError("unexpected login response")
        except (EOFError, ConnectionResetError, BrokenPipeError):
            self.closed = True

    def sendall(self, data):
        for offset in range(0, len(data), 65536):
            self.socket.sendall(packet(b"\x7f" + data[offset:offset + 65536]))

    def recv(self, size):
        while not self.pending and not self.closed:
            try:
                body = socket_frame(self.reader)
                if body[0] == 0x40:
                    self.closed = True
                    break
                if body[0] != 0x7f:
                    raise ValueError("unexpected play packet")
                self.pending.extend(body[1:])
            except EOFError:
                self.closed = True
        result = bytes(self.pending[:size])
        del self.pending[:size]
        return result

    def shutdown(self, how):
        self.socket.shutdown(how)

    def close(self):
        self.reader.close()
        self.socket.close()

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()


async def read_frame(reader):
    length = 0
    for shift in range(0, 35, 7):
        byte = (await reader.readexactly(1))[0]
        if shift == 28 and byte > 15:
            raise ValueError("invalid VarInt")
        length |= (byte & 127) << shift
        if not byte & 128:
            if not 0 < length <= 2 * MIB:
                raise ValueError("invalid frame length")
            return await reader.readexactly(length)
    raise ValueError("invalid VarInt")


async def fixture_client(reader, writer, status=False):
    """A separate process serves every client asynchronously, without a worker cap."""
    stage, received, sent = "login", 0, 0
    name = None
    try:
        if status:
            async with asyncio.timeout(10):
                await read_frame(reader)  # Handshake already validated by the client/proxy.
                if await read_frame(reader) != b"\0":
                    raise ValueError("invalid status request")
                writer.write(STATUS_REPLY)
                await writer.drain()
                # Cache fills close after the response; direct status clients ping.
                ping = await read_frame(reader)
                if len(ping) != 9 or ping[0] != 1:
                    raise ValueError("invalid ping")
                writer.write(varint(len(ping)) + ping)
                await writer.drain()
        else:
            await read_frame(reader)  # Handshake.
            name = login_name(await read_frame(reader))
            writer.write(login_success(name))
            await writer.drain()
            while True:
                stage = "read"
                async with asyncio.timeout(15):
                    data = await read_frame(reader)
                received += len(data)
                stage = "write"
                async with asyncio.timeout(15):
                    writer.write(packet(data))
                    await writer.drain()
                sent += len(data)
    except TimeoutError:
        print(f"fixture timeout: player={name}, stage={stage}, received={received}, sent={sent}, "
              f"write_buffer={writer.transport.get_write_buffer_size()}", flush=True)
    except (OSError, EOFError, ValueError, asyncio.IncompleteReadError):
        pass
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except OSError:
            pass


async def fixture_main():
    echo = await asyncio.start_server(fixture_client, "127.0.0.1", 0, backlog=4096)
    status = await asyncio.start_server(
        lambda reader, writer: fixture_client(reader, writer, status=True),
        "127.0.0.1", 0, backlog=4096,
    )
    print(json.dumps({"echo": echo.sockets[0].getsockname()[1],
                      "status": status.sockets[0].getsockname()[1]}), flush=True)
    async with echo, status:
        await asyncio.Event().wait()


def connect(port):
    sock = socket.create_connection(("127.0.0.1", port), timeout=30)
    sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    return GameSocket(sock, port)


def percentile(samples, fraction):
    """Nearest-rank percentile; missing successes are null, never zero latency."""
    return sorted(samples)[math.ceil(len(samples) * fraction) - 1] if samples else None


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
    return statistics.median(samples), percentile(samples, 0.95)


async def transfer_async(port, size):
    """One event loop owns both directions; failure cancels its blocked peer task."""
    name = new_player_name()
    sent = received = 0
    reader, writer = await asyncio.wait_for(asyncio.open_connection("127.0.0.1", port), 30)
    writer.get_extra_info("socket").setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
    completed = False

    async def send():
        nonlocal sent
        for offset in range(0, size, len(BLOCK)):
            data = BLOCK[:min(len(BLOCK), size - offset)]
            writer.write(packet(b"\x7f" + data))
            await asyncio.wait_for(writer.drain(), 30)
            sent += len(data)
        writer.write_eof()
        await asyncio.wait_for(writer.drain(), 30)

    async def receive():
        nonlocal received
        while received < size:
            body = await asyncio.wait_for(read_frame(reader), 30)
            data = body[1:]
            if body[0] != 0x7f or not data or data != BLOCK[:len(data)]:
                raise ValueError("invalid throughput echo")
            received += len(data)
            if received > size:
                raise ValueError("extra throughput payload")
        # A complete transfer includes the backend's close after our write EOF;
        # neither trailing frames nor a truncated final frame count as success.
        if await asyncio.wait_for(reader.read(1), 30):
            raise ValueError("trailing throughput data")

    try:
        writer.write(handshake(port) + login_start(name))
        await asyncio.wait_for(writer.drain(), 30)
        if packet(await asyncio.wait_for(read_frame(reader), 30)) != login_success(name):
            raise ValueError("unexpected login response")
        async with asyncio.TaskGroup() as tasks:
            tasks.create_task(send())
            tasks.create_task(receive())
        completed = True
        return received
    except Exception as error:
        raise RuntimeError(
            f"transfer failed: player={name}, sent={sent}/{size}, received={received}/{size}"
        ) from error
    finally:
        if completed:
            writer.close()
        else:
            # Do not wait for unsent output after a failed reader/writer. The
            # TaskGroup has already cancelled and awaited the other direction.
            writer.transport.abort()
        try:
            await writer.wait_closed()
        except OSError:
            pass


def transfer(port, size):
    return asyncio.run(transfer_async(port, size))


def throughput(port, clients, size):
    async def transfers():
        async with asyncio.TaskGroup() as tasks:
            for _ in range(clients):
                tasks.create_task(transfer_async(port, size))

    start = time.perf_counter()
    asyncio.run(transfers())
    return clients * size / MIB / (time.perf_counter() - start)


async def receive_exact(sock, size):
    result = bytearray()
    loop = asyncio.get_running_loop()
    while len(result) < size:
        part = await loop.sock_recv(sock, size - len(result))
        if not part:
            raise EOFError("connection closed before ready")
        result.extend(part)
    return bytes(result)


async def setup_attempt(port, scenario, timeout, index, gate):
    cached = scenario == "status_cached"
    name = None if cached else new_player_name()
    await gate.wait()
    started = time.perf_counter()
    setup_ms = None
    error = None
    try:
        async with asyncio.timeout(timeout):
            with socket.socket() as sock:
                sock.setblocking(False)
                sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
                loop = asyncio.get_running_loop()
                await loop.sock_connect(sock, ("127.0.0.1", port))
                payload = handshake(port, 1) + b"\x01\0" if cached else handshake(port) + login_start(name)
                await loop.sock_sendall(sock, payload)
                expected = STATUS_REPLY if cached else login_success(name)
                if await receive_exact(sock, len(expected)) != expected:
                    raise ValueError("response mismatch")
                if not cached:
                    probe = packet(b"\x7f" + PROBE)
                    await loop.sock_sendall(sock, probe)
                    if await receive_exact(sock, len(probe)) != probe:
                        raise ValueError("probe mismatch")
                setup_ms = (time.perf_counter() - started) * 1000
                if cached:
                    ping = b"\x09\x01" + struct.pack(">Q", index)
                    await loop.sock_sendall(sock, ping)
                    if await receive_exact(sock, len(ping)) != ping:
                        raise ValueError("ping mismatch")
    except (OSError, EOFError, ValueError) as exc:
        # Rejections and timeouts are measurements, not reasons to abort a run.
        error = type(exc).__name__
        if isinstance(exc, OSError) and exc.errno is not None:
            error += f":{exc.errno}"
    return {"started": started, "setup_ms": setup_ms if error is None else None,
            "elapsed_ms": (time.perf_counter() - started) * 1000, "error": error}


async def run_bursts(port, scenario, burst_size, attempts, timeout):
    samples, spreads, successes = [], [], []
    for offset in range(0, attempts, burst_size):
        gate = asyncio.Event()
        tasks = [asyncio.create_task(setup_attempt(port, scenario, timeout, index, gate))
                 for index in range(offset, min(offset + burst_size, attempts))]
        await asyncio.sleep(0)  # All tasks reach the gate before releasing this wave.
        gate.set()
        wave = await asyncio.gather(*tasks)
        starts = [sample["started"] for sample in wave]
        spreads.append((max(starts) - min(starts)) * 1000)
        successes.append(sum(sample["error"] is None for sample in wave))
        samples.extend(wave)
    return samples, spreads, successes


def summarize(samples, spreads, successes, elapsed):
    latencies = [sample["setup_ms"] for sample in samples if sample["error"] is None]
    failures = [sample["elapsed_ms"] for sample in samples if sample["error"] is not None]
    return {
        "attempts": len(samples), "succeeded": len(latencies), "failed": len(failures),
        "success_rate": len(latencies) / len(samples), "elapsed_s": elapsed,
        "attempts_per_s": len(samples) / elapsed, "successful_setups_per_s": len(latencies) / elapsed,
        "setup_p50_ms": percentile(latencies, 0.50), "setup_p95_ms": percentile(latencies, 0.95),
        "setup_p99_ms": percentile(latencies, 0.99), "failure_p95_ms": percentile(failures, 0.95),
        "errors": dict(Counter(sample["error"] for sample in samples if sample["error"])),
        "burst_count": len(spreads), "successes_per_burst": successes,
        "launch_spread_p95_ms": percentile(spreads, 0.95), "launch_spread_max_ms": max(spreads),
    }


def process_sample(pid):
    if sys.platform != "linux":
        return None
    # comm can contain spaces and parentheses. Fields after its final ')' start at state (3).
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return {"cpu_s": (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"),
            "rss_mib": int(fields[21]) * os.sysconf("SC_PAGE_SIZE") / MIB}


class Resources:
    def __init__(self, pids):
        self.pids = pids
        self.stop = threading.Event()
        self.failure = None

    def sample(self):
        return {name: process_sample(pid) if pid else None for name, pid in self.pids.items()}

    def poll(self):
        try:
            while not self.stop.wait(SAMPLE_INTERVAL):
                for name, value in self.sample().items():
                    if value:
                        self.peaks[name] = max(self.peaks[name], value["rss_mib"])
        except Exception as exc:
            self.failure = exc

    def __enter__(self):
        self.before = self.sample()
        self.peaks = {name: value["rss_mib"] for name, value in self.before.items() if value}
        self.started = time.perf_counter()
        self.thread = threading.Thread(target=self.poll)
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.thread.join()
        self.elapsed = time.perf_counter() - self.started
        after = self.sample()
        if self.failure:
            raise self.failure
        self.result = {}
        for name, value in after.items():
            self.result[name] = None if value is None else {
                "cpu_s": value["cpu_s"] - self.before[name]["cpu_s"],
                "cpu_percent": 100 * (value["cpu_s"] - self.before[name]["cpu_s"]) / self.elapsed,
                "rss_start_mib": self.before[name]["rss_mib"], "rss_end_mib": value["rss_mib"],
                "rss_peak_mib": max(self.peaks[name], value["rss_mib"]),
            }


def metrics(port):
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=5)
    try:
        connection.request("GET", "/metrics")
        response = connection.getresponse()
        if response.status != 200:
            raise RuntimeError(f"metrics returned {response.status}")
        return {name: int(value) for name, value in re.findall(
            r"^(rift_\w+) (\d+)$", response.read().decode(), re.MULTILINE)}
    finally:
        connection.close()


def idle_metrics(port):
    deadline = time.monotonic() + 10
    while True:
        result = metrics(port)
        if result["rift_connections_active"] == 0:
            return result
        if time.monotonic() >= deadline:
            raise TimeoutError("proxy did not drain benchmark clients")
        time.sleep(0.005)


def configuration(scenario, backend, init_iterations):
    policy = '"backend"' if scenario == "rift" else f'{{ ["{HOSTNAME}"] = "backend" }}'
    initialization = ""
    if scenario == "lua_init":
        initialization = (f"local entries = {{}}\nfor i = 1, {init_iterations} do entries[i] = i end\n"
                          f"assert(#entries == {init_iterations})\n")
    hook = ""
    if scenario in ("lua", "lua_init"):
        hook = 'on_route = function(connection) assert(connection.listener == "public"); return nil end,'
    cache = "status_cache = { ttl_ms = 86400000 }," if scenario == "status_cached" else ""
    return initialization + f'''return {{
    listeners = {{ public = "127.0.0.1:0" }},
    backends = {{ backend = "127.0.0.1:{backend}" }},
    routes = {{ public = {policy} }},
    metrics = "127.0.0.1:0",
    {hook}
    {cache}
}}
'''


@contextmanager
def proxy_for(binary, directory, scenario, backend, init_iterations):
    source = configuration(scenario, backend, init_iterations)
    config = directory / f"{scenario}.lua"
    config.write_text(source)
    log = directory / f"{scenario}.log"
    with process([str(binary), "--config", str(config)], directory, log.name) as proxy:
        wait_ready(proxy, lambda: "rift: metrics on" in log.read_text(), log)
        contents = log.read_text()
        port = int(re.search(r"rift: listening on 127\.0\.0\.1:(\d+)", contents)[1])
        metrics_port = int(re.search(r"rift: metrics on 127\.0\.0\.1:(\d+)", contents)[1])
        yield proxy.pid, port, metrics_port, log, source


def number(value):
    return "n/a" if value is None else f"{value:.2f}"


def measure_route(port, scenario, pids, args, metrics_port=None, log=None):
    route = {"setup_bursts": []}
    if scenario in ("direct", "rift") and not args.skip_throughput:
        p50, p95 = latency(port)
        single = statistics.median(throughput(port, 1, 64 * MIB) for _ in range(3))
        parallel = statistics.median(throughput(port, 16, 16 * MIB) for _ in range(3))
        route.update(rtt_p50_us=p50, rtt_p95_us=p95, single_mib_s=single, parallel_mib_s=parallel)
        print(f"{scenario}: RTT p50/p95 {p50:.1f}/{p95:.1f} us; throughput 1/16 clients "
              f"{single:.1f}/{parallel:.1f} MiB/s", flush=True)
    for size in args.burst_sizes:
        # Sequential warmup primes the cache and exercises the connection startup path.
        warmup, _, _ = asyncio.run(run_bursts(port, scenario, 1, 16, args.timeout))
        if any(sample["error"] for sample in warmup):
            raise RuntimeError(f"{scenario}: sequential warmup failed: {warmup}")
        before = idle_metrics(metrics_port) if metrics_port else {}
        log_offset = log.stat().st_size if log else 0
        with Resources(pids) as resources:
            start = time.perf_counter()
            samples, spreads, successes = asyncio.run(
                run_bursts(port, scenario, size, args.attempts, args.timeout))
            elapsed = time.perf_counter() - start
        result = summarize(samples, spreads, successes, elapsed)
        result.update(burst_size=size, resources=resources.result,
                      resource_window_s=resources.elapsed)
        after = idle_metrics(metrics_port) if metrics_port else {}
        result["proxy_counters"] = {key: value - before[key] for key, value in after.items()
                                     if key.endswith("_total")}
        if log:
            with log.open("rb") as stream:
                stream.seek(log_offset)
                text = stream.read().decode(errors="replace")
            result["lua_errors"] = {
                "capacity_exhausted": text.count("script capacity exhausted"),
                "deadline_exceeded": text.count("script deadline exceeded"),
                "instruction_limit": text.count("script instruction limit exceeded"),
            }
        if scenario == "status_cached":
            counts = result["proxy_counters"]
            result["cache_verified"] = (counts["rift_status_cache_misses_total"] == 0
                                        and counts["rift_status_cache_hits_total"] >= result["succeeded"])
            if not result["cache_verified"]:
                raise RuntimeError(f"cached-status run did not hit the warm cache: {counts}")
        route["setup_bursts"].append(result)
        usage = resources.result["proxy"] or {}
        print(f"{scenario:14} burst={size:4} success={result['succeeded']:5}/{args.attempts} "
              f"({100 * result['success_rate']:6.2f}%) "
              f"p95/p99={number(result['setup_p95_ms'])}/{number(result['setup_p99_ms'])} ms "
              f"ok/s={result['successful_setups_per_s']:8.0f} "
              f"CPU={number(usage.get('cpu_percent'))}% "
              f"RSS={number(usage.get('rss_peak_mib'))} MiB "
              f"Lua busy={result.get('lua_errors', {}).get('capacity_exhausted', 0)}", flush=True)
    return route


def positive_int(value):
    result = int(value)
    if result <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return result


def burst_sizes(value):
    try:
        sizes = [positive_int(part) for part in value.split(",")]
    except ValueError as exc:
        raise argparse.ArgumentTypeError("expected comma-separated positive integers") from exc
    if max(sizes) > 4096:
        raise argparse.ArgumentTypeError("burst sizes must not exceed 4096")
    return sizes


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", type=Path, help="write machine-readable results for comparisons")
    parser.add_argument("--burst-sizes", type=burst_sizes, default=burst_sizes("1,4,16,64,256"))
    parser.add_argument("--attempts", type=positive_int, default=2048,
                        help="connections per scenario and burst size (default: 2048)")
    parser.add_argument("--timeout", type=float, default=5, help="whole attempt deadline in seconds")
    parser.add_argument("--lua-init-iterations", type=int, default=10000,
                        help="table entries rebuilt by lua_init (0..15000; default: 10000)")
    parser.add_argument("--scenarios", nargs="+", choices=SCENARIOS, default=list(SCENARIOS))
    parser.add_argument("--skip-throughput", action="store_true", help="only measure connection bursts")
    parser.add_argument("--binary", type=Path, help="use an existing Rift binary instead of building")
    parser.add_argument("--fixture", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if not __debug__:
        parser.error("do not use python -O: transfer assertions must be enabled")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be finite and positive")
    if not 0 <= args.lua_init_iterations <= 15000:
        parser.error("--lua-init-iterations must be in 0..15000")
    if args.attempts < max(args.burst_sizes):
        parser.error("--attempts must cover the largest burst")
    if args.fixture:
        asyncio.run(fixture_main())
        return
    binary = args.binary.resolve() if args.binary else ROOT / "target/release/rift"
    if not args.binary:
        subprocess.run(["cargo", "build", "--release", "--locked"], cwd=ROOT, check=True)
    report = {
        "schema_version": 2, "platform": platform.platform(),
        "recorded_at": datetime.now(timezone.utc).isoformat(), "python": platform.python_version(),
        "git_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "binary": str(binary), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "benchmark_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
        "cpu_count": os.cpu_count(), "tokio_worker_threads": os.environ.get("TOKIO_WORKER_THREADS"),
        "cpu_affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
        "settings": {"burst_sizes": args.burst_sizes, "attempts_per_size": args.attempts,
                     "timeout_s": args.timeout, "lua_init_iterations": args.lua_init_iterations,
                     "lua_max_concurrent": 4, "lua_state": "fresh VM and config evaluation per connection",
                     "warmup_connections_per_size": 16, "resource_sample_interval_s": SAMPLE_INTERVAL,
                     "cpu_clock_ticks_per_s": os.sysconf("SC_CLK_TCK") if sys.platform == "linux" else None},
        "method": {
            "setup": "TCP connect through verified login+play-packet echo, or complete cached status response",
            "throughput": "one asyncio event loop owns every connection with concurrent send/receive tasks; verifies payload and EOF; replaces threaded buffered socket driver",
            "status_success": "also requires a correct per-client ping/pong after the status response",
            "percentiles": "nearest rank over successful attempts only; no retries",
            "bursts": "gate-released asyncio clients; wait for the entire wave before releasing the next",
            "resources": "per-process CPU seconds and percent (100%=one core), RSS sampled every 5ms; Linux only",
            "scope": "loopback including Python client/backend overhead; not Minecraft player capacity",
        },
        "routes": {},
    }
    print("Setup latency includes routing and a verified response; percentiles cover successes only.", flush=True)
    print("CPU/RSS are for Rift; direct has no proxy. JSON also records fixture and driver resources.", flush=True)
    with tempfile.TemporaryDirectory(prefix="rift-bench-") as temp:
        directory = Path(temp)
        log = directory / "fixture.log"
        with process([sys.executable, str(Path(__file__).resolve()), "--fixture"],
                     directory, log.name) as fixture:
            try:
                wait_ready(fixture, lambda: "\n" in log.read_text(), log)
                ports = json.loads(log.read_text().splitlines()[0])
                for scenario in args.scenarios:
                    backend = ports["status" if scenario == "status_cached" else "echo"]
                    pids = {"proxy": None, "fixture": fixture.pid, "driver": os.getpid()}
                    if scenario == "direct":
                        route = measure_route(backend, scenario, pids, args)
                    else:
                        with proxy_for(binary, directory, scenario, backend, args.lua_init_iterations) as proxy:
                            pid, port, metrics_port, proxy_log, source = proxy
                            pids["proxy"] = pid
                            route = measure_route(port, scenario, pids, args, metrics_port, proxy_log)
                            route["config_source"] = source
                    report["routes"][scenario] = route
                    if args.report:
                        args.report.parent.mkdir(parents=True, exist_ok=True)
                        args.report.write_text(json.dumps(report, indent=2) + "\n")
            except Exception:
                print(f"Fixture log:\n{log.read_text()[-8000:]}", file=sys.stderr)
                raise


if __name__ == "__main__":
    main()
