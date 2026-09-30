#!/usr/bin/env python3
"""Compare Rift and pinned Velocity with identical offline protocol-47 traffic.

Python 3.11+ and Linux /proc are required for published CPU/RSS measurements.
The backend is a protocol fixture, not a Minecraft world simulation. No results
from this harness establish production player capacity or authenticated cost.
"""

import argparse
import asyncio
from collections import Counter
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import io
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid

from bench import (HOSTNAME, LOOPBACK_MSS, Resources, SAMPLE_INTERVAL, fixture_server,
                   handshake, login_name, login_start, login_success, new_player_name,
                   packet, percentile, read_frame, tcp_socket)
from minecraft import ROOT, process, read_varint, string, unused_port, wait_ready

CHANNEL = "rift:bench"
CHANNEL_BYTES = string(CHANNEL)
PROBE = b"rift-velocity-comparison-ready"
# Minecraft 1.8.9: entity id, game mode, dimension, difficulty, max players,
# level type, reduced debug information. This transitions Velocity to PLAY.
JOIN_BODY = b"\x01" + struct.pack(">iBbBB", 1, 0, 0, 0, 255) + string("default") + b"\0"
STATUS = {"version": {"name": "comparison fixture 1.8.9", "protocol": 47},
          "players": {"max": 10000, "online": 0},
          "description": {"text": "rift-velocity-comparison"}}


def unpack(body):
    stream = io.BytesIO(body)
    packet_id = read_varint(stream)
    return packet_id, stream


def read_string(stream, maximum=32767):
    length = read_varint(stream)
    if not 0 <= length <= maximum:
        raise ValueError("invalid string length")
    data = stream.read(length)
    if len(data) != length:
        raise EOFError("truncated string")
    return data.decode("utf-8")


def error_name(error):
    name = type(error).__name__
    if isinstance(error, OSError) and error.errno is not None:
        name += f":{error.errno}"
    return name


async def close_writer(writer):
    writer.close()
    try:
        await asyncio.wait_for(writer.wait_closed(), 1)
    except (OSError, TimeoutError):
        writer.transport.abort()


async def fixture_client(reader, writer):
    """Minimal valid 1.8.9 login, Join Game, and custom plugin-message echo."""
    try:
        async with asyncio.timeout(15):
            packet_id, fields = unpack(await read_frame(reader))
            if packet_id != 0 or read_varint(fields) != 47:
                raise ValueError("fixture requires protocol 47 handshake")
            read_string(fields, 255)
            if len(fields.read(2)) != 2:
                raise EOFError("missing handshake port")
            state = read_varint(fields)
            if fields.read() or state not in (1, 2):
                raise ValueError("invalid handshake state")
            if state == 1:
                if await read_frame(reader) != b"\0":
                    raise ValueError("invalid status request")
                writer.write(packet(b"\0" + string(json.dumps(STATUS, separators=(",", ":")))))
                await writer.drain()
                ping = await read_frame(reader)
                if len(ping) != 9 or ping[0] != 1:
                    raise ValueError("invalid status ping")
                writer.write(packet(ping))
                await writer.drain()
                return
            name = login_name(await read_frame(reader))
            writer.write(login_success(name) + packet(JOIN_BODY))
            await writer.drain()
        while True:
            async with asyncio.timeout(120):
                packet_id, fields = unpack(await read_frame(reader))
            if packet_id == 0x17:
                channel = read_string(fields, 20)
                payload = fields.read()
                if channel == CHANNEL:
                    if not payload or len(payload) > 32767:
                        raise ValueError("invalid fixture payload length")
                    writer.write(packet(b"\x3f" + CHANNEL_BYTES + payload))
                    await writer.drain()
                # Velocity sends its own brand / registration messages.
            elif packet_id not in (0x00, 0x15):
                raise ValueError(f"unexpected client play packet {packet_id:#x}")
    except asyncio.IncompleteReadError as error:
        if error.partial or error.expected != 1:
            print(f"fixture truncated frame: {error}", flush=True)
    except (OSError, EOFError, ValueError, TimeoutError) as error:
        print(f"fixture {error_name(error)}: {error}", flush=True)
    finally:
        await close_writer(writer)


async def fixture_main():
    async with await fixture_server(fixture_client) as server:
        print(json.dumps({"backend": server.sockets[0].getsockname()[1]}), flush=True)
        await asyncio.Event().wait()


class GameConnection:
    def __init__(self, reader, writer):
        self.reader, self.writer = reader, writer

    async def receive(self):
        """Read a play packet, transparently replying to protocol keepalives."""
        while True:
            packet_id, fields = unpack(await read_frame(self.reader))
            body = fields.read()
            if packet_id == 0x00:
                self.writer.write(packet(b"\0" + body))
                await self.writer.drain()
            elif packet_id == 0x40:
                raise ValueError(f"play disconnect: {body[:160]!r}")
            else:
                return packet_id, body

    async def echo(self, payload, timeout):
        async with asyncio.timeout(timeout):
            self.writer.write(packet(b"\x17" + CHANNEL_BYTES + payload))
            await self.writer.drain()
            while True:
                packet_id, body = await self.receive()
                if packet_id == 0x3f:
                    fields = io.BytesIO(body)
                    channel = read_string(fields, 20)
                    if channel == CHANNEL:
                        if fields.read() != payload:
                            raise ValueError("echo payload mismatch")
                        return
                elif packet_id not in (0x01, 0x38, 0x45, 0x47):
                    raise ValueError(f"unexpected server play packet {packet_id:#x}")

    async def close(self):
        await close_writer(self.writer)


async def open_game(port, timeout):
    sock = tcp_socket()
    sock.setblocking(False)
    writer = None
    try:
        async with asyncio.timeout(timeout):
            await asyncio.get_running_loop().sock_connect(sock, ("127.0.0.1", port))
            reader, writer = await asyncio.open_connection(sock=sock)
            sock.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            name = new_player_name()
            writer.write(handshake(port) + login_start(name))
            await writer.drain()
            if packet(await read_frame(reader)) != login_success(name):
                raise ValueError("unexpected login response (compression must be disabled)")
            game = GameConnection(reader, writer)
            while True:
                packet_id, body = await game.receive()
                if packet_id == 0x01:
                    if b"\x01" + body != JOIN_BODY:
                        raise ValueError("unexpected Join Game response")
                    return game
                if packet_id not in (0x3f, 0x38, 0x45, 0x47):
                    raise ValueError(f"unexpected packet before Join Game: {packet_id:#x}")
    except BaseException:
        if writer is None:
            sock.close()
        else:
            await close_writer(writer)
        raise


async def login_attempt(port, timeout, index, gate=None):
    if gate is not None:
        await gate.wait()
    started = time.perf_counter()
    game = None
    error = detail = None
    latency = None
    try:
        async with asyncio.timeout(timeout):
            game = await open_game(port, timeout)
            await game.echo(PROBE, timeout)
            latency = (time.perf_counter() - started) * 1000
    except (OSError, EOFError, ValueError, TimeoutError) as exc:
        error, detail = error_name(exc), str(exc)[:240]
    finally:
        if game is not None:
            await game.close()
    return {"index": index, "started": started, "latency_ms": latency,
            "elapsed_ms": (time.perf_counter() - started) * 1000,
            "error": error, "detail": detail}


async def run_bursts(port, attempts, burst_size, timeout):
    samples, spreads = [], []
    for offset in range(0, attempts, burst_size):
        gate = asyncio.Event()
        tasks = [asyncio.create_task(login_attempt(port, timeout, index, gate))
                 for index in range(offset, min(offset + burst_size, attempts))]
        await asyncio.sleep(0)
        gate.set()
        wave = await asyncio.gather(*tasks)
        starts = [sample["started"] for sample in wave]
        spreads.append((max(starts) - min(starts)) * 1000)
        samples.extend(wave)
    return samples, spreads


def summarize(samples, elapsed):
    successful = [sample["latency_ms"] for sample in samples if sample["error"] is None]
    failures = [sample["elapsed_ms"] for sample in samples if sample["error"] is not None]
    return {"attempts": len(samples), "succeeded": len(successful), "failed": len(failures),
            "failure_rate": len(failures) / len(samples) if samples else None,
            "elapsed_s": elapsed, "successful_per_s": len(successful) / elapsed if elapsed else None,
            "latency_p50_ms": percentile(successful, 0.50),
            "latency_p95_ms": percentile(successful, 0.95),
            "latency_p99_ms": percentile(successful, 0.99),
            "failure_p95_ms": percentile(failures, 0.95),
            "errors": dict(Counter(sample["error"] for sample in samples if sample["error"]))}


async def open_clients(port, clients, timeout):
    async def connect(index):
        started = time.perf_counter()
        try:
            async with asyncio.timeout(timeout):
                game = await open_game(port, timeout)
                await game.echo(PROBE, timeout)
            return game, {"index": index, "error": None,
                          "latency_ms": (time.perf_counter() - started) * 1000,
                          "elapsed_ms": (time.perf_counter() - started) * 1000}
        except (OSError, EOFError, ValueError, TimeoutError) as error:
            if "game" in locals():
                await game.close()
            return None, {"index": index, "error": error_name(error), "latency_ms": None,
                          "elapsed_ms": (time.perf_counter() - started) * 1000,
                          "detail": str(error)[:240]}
    return await asyncio.gather(*(connect(index) for index in range(clients)))


async def run_echo(clients, duration, rate, payload_bytes, timeout):
    """A fixed offered schedule; failed sessions and missed slots stay in counts.

    Each client has at most one outstanding request. A slow response cannot
    silently lower the denominator: missed slots are recorded as failures.
    Latency starts at the scheduled time and therefore includes driver delay.
    """
    started = time.perf_counter() + 0.01
    count = math.ceil(duration * rate)
    period = 1 / rate

    async def client_worker(index, game):
        samples = []
        unavailable = game is None
        for sequence in range(count):
            scheduled = started + sequence * period
            await asyncio.sleep(max(0, scheduled - time.perf_counter()))
            dispatched = time.perf_counter()
            delay_ms = (dispatched - scheduled) * 1000
            error = detail = None
            latency = rtt = None
            if unavailable:
                error = "connection_unavailable"
            elif dispatched >= scheduled + period:
                error = "schedule_overrun"
            else:
                payload = struct.pack(">II", index, sequence) + b"x" * (payload_bytes - 8)
                try:
                    await game.echo(payload, timeout)
                    completed = time.perf_counter()
                    latency = (completed - scheduled) * 1000
                    rtt = (completed - dispatched) * 1000
                except (OSError, EOFError, ValueError, TimeoutError) as exc:
                    error, detail = error_name(exc), str(exc)[:240]
                    unavailable = True
            samples.append({"client": index, "sequence": sequence,
                            "scheduled_ms": sequence * period * 1000,
                            "launch_delay_ms": delay_ms, "latency_ms": latency,
                            "rtt_ms": rtt, "elapsed_ms": (time.perf_counter() - dispatched) * 1000,
                            "error": error, "detail": detail})
        return samples

    waves = await asyncio.gather(*(client_worker(index, game)
                                   for index, game in enumerate(clients)))
    # Retain a full duration resource window, including the last interarrival gap.
    await asyncio.sleep(max(0, started + duration - time.perf_counter()))
    return [sample for wave in waves for sample in wave]


async def measure_echo(port, args, pids, duration):
    connections = await open_clients(port, args.clients, args.timeout)
    games = [game for game, _ in connections]
    setup = [sample for _, sample in connections]
    try:
        with Resources(pids) as resources:
            started = time.perf_counter()
            samples = await run_echo(games, duration, args.rate, args.payload_bytes, args.timeout)
            elapsed = time.perf_counter() - started
        result = summarize(samples, elapsed)
        rtts = [sample["rtt_ms"] for sample in samples if sample["error"] is None]
        result.update(resources=resources.result, resource_window_s=resources.elapsed,
                      rtt_p50_ms=percentile(rtts, .50), rtt_p95_ms=percentile(rtts, .95),
                      rtt_p99_ms=percentile(rtts, .99),
                      launch_delay_p95_ms=percentile([s["launch_delay_ms"] for s in samples], .95),
                      successful_payload_mib_s=result["succeeded"] * args.payload_bytes / 1024**2 / elapsed,
                      setup=setup, samples=samples)
        return result
    finally:
        await asyncio.gather(*(game.close() for game in games if game is not None))


def configurations(port, backend):
    rift = f'''return {{
    listeners = {{ public = "127.0.0.1:{port}" }},
    backends = {{ backend = "127.0.0.1:{backend}" }},
    routes = {{ public = "backend" }},
    limits = {{ max_connections = 16384 }},
}}
'''
    velocity = f'''config-version = "2.9"
bind = "127.0.0.1:{port}"
motd = "rift-velocity-comparison"
show-max-players = 10000
online-mode = false
force-key-authentication = false
prevent-client-proxy-connections = false
player-info-forwarding-mode = "none"
announce-forge = false
kick-existing-players = false
enable-player-address-logging = false

[servers]
backend = "127.0.0.1:{backend}"
try = ["backend"]

[forced-hosts]

[ping-passthrough]
version = false
players = false
description = false
favicon = false
modinfo = false

[advanced]
compression-threshold = -1
compression-level = -1
login-ratelimit = 0
connection-timeout = 5000
read-timeout = 120000
haproxy-protocol = false
tcp-fast-open = false
bungee-plugin-message-channel = false
show-ping-requests = false
failover-on-unexpected-server-disconnect = false
announce-proxy-commands = false
log-command-executions = false
log-player-connections = false
accepts-transfers = false

[query]
enabled = false

[packet-limiter]
packets-per-second = -1
bytes-per-second = -1
decompressed-bytes-per-second = -1
'''
    return {"rift": rift, "velocity": velocity}


def listening(port):
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=.2):
            return True
    except OSError:
        return False


def verify_velocity_config(source, metrics_source):
    config = tomllib.loads(source)
    required = {"online-mode": False, "force-key-authentication": False,
                "player-info-forwarding-mode": "none"}
    for key, expected in required.items():
        if config.get(key) != expected:
            raise ValueError(f"Velocity effective config changed {key}")
    for section, required in (
        ("advanced", {"compression-threshold": -1, "login-ratelimit": 0,
                      "bungee-plugin-message-channel": False, "log-player-connections": False,
                      "failover-on-unexpected-server-disconnect": False}),
        ("packet-limiter", {"packets-per-second": -1, "bytes-per-second": -1,
                            "decompressed-bytes-per-second": -1}),
    ):
        for key, expected in required.items():
            if config.get(section, {}).get(key) != expected:
                raise ValueError(f"Velocity effective config changed {section}.{key}")
    if not re.search(r"(?m)^enabled\s*=\s*false\s*$", metrics_source):
        raise ValueError("Velocity bStats was not disabled")


@contextmanager
def launch_proxy(name, directory, backend, args):
    port = unused_port()
    sources = configurations(port, backend)
    environment = os.environ.copy()
    environment["TOKIO_WORKER_THREADS"] = str(args.workers)
    if name == "rift":
        config = directory / "rift.lua"
        config.write_text(sources[name])
        command = [str(args.binary), "--config", str(config)]
    else:
        config = directory / "velocity.toml"
        config.write_text(sources[name])
        (directory / "plugins" / "bStats").mkdir(parents=True)
        (directory / "plugins" / "bStats" / "config.txt").write_text(
            f"enabled=false\nserver-uuid={uuid.uuid4()}\nlog-errors=false\n"
            "log-sent-data=false\nlog-response-status-text=false\n")
        command = [args.java, f"-XX:ActiveProcessorCount={args.workers}",
                   f"-Dio.netty.eventLoopThreads={args.workers}",
                   f"-Xms{args.java_heap_mib}M", f"-Xmx{args.java_heap_mib}M", "-jar", str(args.velocity_jar)]
    log = directory / "proxy.log"
    with process(command, directory, log.name, env=environment) as proxy:
        wait_ready(proxy, lambda: listening(port), log, timeout=90)
        metrics_source = None
        if name == "velocity":
            metrics_source = (directory / "plugins" / "bStats" / "config.txt").read_text()
            verify_velocity_config(config.read_text(), metrics_source)
        yield proxy, port, {"command": command, "config_source": sources[name],
                            "config_sha256": hashlib.sha256(sources[name].encode()).hexdigest(),
                            "effective_config_source": config.read_text(),
                            "effective_config_sha256": sha256(config), "log": str(log),
                            "metrics_config_source": metrics_source,
                            "tokio_worker_threads": environment["TOKIO_WORKER_THREADS"]}


def sha256(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def positive_int(value):
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def positive_float(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and positive")
    return number


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/rift")
    parser.add_argument("--velocity-jar", type=Path)
    parser.add_argument("--velocity-manifest", type=Path, default=ROOT / "tests/velocity.json")
    parser.add_argument("--java", default="java")
    parser.add_argument("--report", type=Path)
    parser.add_argument("--work-dir", type=Path, help="retain configs/logs here (must not already exist)")
    parser.add_argument("--trials", type=positive_int, default=3)
    parser.add_argument("--clients", type=positive_int, default=16)
    parser.add_argument("--workers", type=positive_int, default=2)
    parser.add_argument("--java-heap-mib", type=positive_int, default=1024)
    parser.add_argument("--duration", type=positive_float, default=15)
    parser.add_argument("--warmup", type=positive_float, default=15)
    parser.add_argument("--rate", type=positive_float, default=20,
                        help="offered echo requests per client per second")
    parser.add_argument("--payload-bytes", type=positive_int, default=1024)
    parser.add_argument("--attempts", type=positive_int, default=256)
    parser.add_argument("--burst-size", type=positive_int, default=16)
    parser.add_argument("--timeout", type=positive_float, default=5)
    parser.add_argument("--require-success", action="store_true",
                        help="exit nonzero if measured or warmup requests fail (for CI)")
    parser.add_argument("--fixture", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    if args.fixture:
        return args
    if args.velocity_jar is None or args.report is None:
        parser.error("--velocity-jar and --report are required")
    if not 8 <= args.payload_bytes <= 32767:
        parser.error("--payload-bytes must be in 8..32767 for protocol 47 plugin messages")
    if args.clients > 4096 or args.burst_size > 4096:
        parser.error("--clients and --burst-size must not exceed 4096")
    if args.attempts < args.burst_size:
        parser.error("--attempts must cover at least one full burst")
    if args.duration * args.rate < 1 or args.warmup * args.rate < 1:
        parser.error("--duration and --warmup must each offer at least one request per client")
    args.binary = args.binary.resolve(strict=True)
    args.velocity_jar = args.velocity_jar.resolve(strict=True)
    args.java = shutil.which(args.java)
    if args.java is None:
        parser.error("Java executable not found")
    args.java = str(Path(args.java).resolve())
    args.report = args.report.resolve()
    return args


def aggregate(results):
    """Summaries are medians of per-trial values; raw trials remain authoritative."""
    aggregate_results = {}
    for proxy in ("rift", "velocity"):
        proxy_results = [result for result in results if result["proxy"] == proxy]
        aggregate_results[proxy] = {}
        for workload in ("login", "echo"):
            trials = [result[workload] for result in proxy_results if workload in result]
            if not trials:
                continue
            summary = {"trials": len(trials)}
            values = {key: [trial[key] for trial in trials]
                      for key in ("latency_p50_ms", "latency_p95_ms", "latency_p99_ms", "successful_per_s")}
            for key in ("cpu_s", "cpu_percent", "rss_peak_mib"):
                values[key] = [trial["resources"]["proxy"].get(key)
                               if trial["resources"]["proxy"] else None for trial in trials]
            for key, samples in values.items():
                samples = [value for value in samples if value is not None]
                summary[key] = {"median": statistics.median(samples), "min": min(samples),
                                "max": max(samples)} if samples else None
            summary["attempts"] = sum(trial["attempts"] for trial in trials)
            summary["failed"] = sum(trial["failed"] for trial in trials)
            summary["failure_rate"] = summary["failed"] / summary["attempts"]
            aggregate_results[proxy][workload] = summary
    return aggregate_results


def write_report(report, path):
    path.parent.mkdir(parents=True, exist_ok=True)
    pending = path.with_name(path.name + ".tmp")
    pending.write_text(json.dumps(report, indent=2, allow_nan=False) + "\n")
    pending.replace(path)


def report_has_failures(report):
    return any(
        result[workload]["failed"] or any(sample["error"] for sample in result[workload].get("setup", []))
        for result in report["results"]
        for workload in ("login", "echo", "warmup_login", "warmup_echo")
    )


def command_output(command):
    return subprocess.check_output(command, cwd=ROOT, text=True, stderr=subprocess.STDOUT).strip()


def host_info():
    cpu_model = None
    if Path("/proc/cpuinfo").exists():
        match = re.search(r"^model name\s*:\s*(.+)$", Path("/proc/cpuinfo").read_text(), re.M)
        cpu_model = match[1] if match else None
    return {"platform": platform.platform(), "machine": platform.machine(),
            "cpu_model": cpu_model, "logical_cpus": os.cpu_count(),
            "cpu_affinity": sorted(os.sched_getaffinity(0)) if hasattr(os, "sched_getaffinity") else None,
            "python": platform.python_version(),
            "load_average": list(os.getloadavg()) if hasattr(os, "getloadavg") else None,
            "cpu_clock_ticks_per_s": os.sysconf("SC_CLK_TCK") if sys.platform == "linux" else None,
            "cgroup_cpu_max": Path("/sys/fs/cgroup/cpu.max").read_text().strip()
                if Path("/sys/fs/cgroup/cpu.max").exists() else None,
            "cgroup_memory_max": Path("/sys/fs/cgroup/memory.max").read_text().strip()
                if Path("/sys/fs/cgroup/memory.max").exists() else None}


def main(argv=None):
    args = parse_args(argv)
    if args.fixture:
        asyncio.run(fixture_main())
        return
    manifest = json.loads(args.velocity_manifest.read_text())
    expected = manifest.get("sha256") or manifest.get("checksum")
    actual = sha256(args.velocity_jar)
    if actual != expected:
        raise SystemExit(f"Velocity SHA-256 mismatch: expected {expected}, got {actual}")
    if args.work_dir:
        directory = args.work_dir.resolve()
        directory.mkdir(parents=True, exist_ok=False)
    else:
        runs = ROOT / "target/velocity-comparison/runs"
        runs.mkdir(parents=True, exist_ok=True)
        directory = Path(tempfile.mkdtemp(prefix="comparison-", dir=runs))
    script = Path(__file__).resolve()
    report = {
        "schema_version": 1, "status": "running",
        "recorded_at": datetime.now(timezone.utc).isoformat(), "host": host_info(),
        "provenance": {
            "command": [sys.executable, *sys.argv], "git_commit": command_output(["git", "rev-parse", "HEAD"]),
            "git_dirty": bool(command_output(["git", "status", "--porcelain"])),
            "binary": str(args.binary), "binary_sha256": sha256(args.binary),
            "velocity": manifest, "velocity_jar_sha256": actual,
            "java_version": command_output([args.java, "-version"]),
            "java_binary_sha256": sha256(args.java),
            "java_release": (Path(args.java).parents[1] / "release").read_text()
                if (Path(args.java).parents[1] / "release").is_file() else None,
            "sources_sha256": {name: sha256(script.parent / name) for name in
                                ("compare_velocity.py", "bench.py", "minecraft.py", "prometheus.py", "velocity.json")},
            "logs": str(directory)},
        "settings": {key: getattr(args, key) for key in
                     ("trials", "clients", "workers", "java_heap_mib", "duration", "warmup", "rate",
                      "payload_bytes", "attempts", "burst_size", "timeout")},
        "method": {
            "scope": "Loopback, synthetic Minecraft 1.8.9 protocol fixture; no world ticks, chunks, online authentication, encryption, compression, plugins, or forwarding identity.",
            "equivalence": "Same backend fixture, clients, packet bytes, offered rates, deadlines, worker setting and CPU affinity; fresh proxy and fixture per trial; alternate Rift/Velocity first each trial.",
            "login": "TCP connect through Login Success, Join Game and verified plugin-message echo; fixed attempts in gate-released waves; no retries.",
            "echo": "Fixed per-client schedule after login; at most one outstanding request per client. Missed slots and all remaining slots of failed sessions count as failures. Both scheduled latency and send-to-response RTT are recorded.",
            "latency": "Nearest-rank percentiles over successful operations only; failures retained in denominators and raw samples. Echo latency includes dispatch delay from the offered schedule.",
            "resources": "Linux /proc process CPU user+system seconds; 100% CPU is one logical core. RSS sampled every 5ms, includes JVM native memory; driver/backend separately recorded, excluded from proxy totals. Startup/setup outside echo window; trailing responses inside.",
            "warmup": "One complete login burst pass and fixed-duration echo pass on every fresh proxy; warmup samples retained separately and excluded from published measured summaries.",
            "aggregation": "Median/min/max of per-trial metrics, pooled failure counts; no cross-trial percentile averaging.",
            "limitations": "Shared host and Python client/backend may limit load; three short trials do not establish long-running JVM steady state, peak capacity or production player latency.",
            "resource_sample_interval_s": SAMPLE_INTERVAL, "tcp_maxseg_bytes": LOOPBACK_MSS},
        "results": [],
    }
    write_report(report, args.report)
    try:
        for trial in range(args.trials):
            order = ("rift", "velocity") if trial % 2 == 0 else ("velocity", "rift")
            for position, name in enumerate(order):
                trial_directory = directory / f"{trial + 1}-{name}"
                trial_directory.mkdir()
                log = trial_directory / "fixture.log"
                result = {"trial": trial + 1, "position": position + 1, "proxy": name,
                          "fixture_log": str(log)}
                report["results"].append(result)
                print(f"Trial {trial + 1}/{args.trials}: {name}, warming up...", flush=True)
                with process([sys.executable, str(script), "--fixture"], trial_directory, log.name) as fixture:
                    wait_ready(fixture, lambda: "\n" in log.read_text(), log)
                    backend = json.loads(log.read_text().splitlines()[0])["backend"]
                    with launch_proxy(name, trial_directory, backend, args) as (proxy, port, meta):
                        result.update(meta)
                        pids = {"proxy": proxy.pid, "fixture": fixture.pid, "driver": os.getpid()}
                        started = time.perf_counter()
                        warmup_samples, _ = asyncio.run(run_bursts(port, args.attempts, args.burst_size, args.timeout))
                        result["warmup_login"] = summarize(warmup_samples, time.perf_counter() - started)
                        result["warmup_login"]["samples"] = warmup_samples
                        result["warmup_echo"] = asyncio.run(measure_echo(port, args, pids, args.warmup))
                        with Resources(pids) as resources:
                            started = time.perf_counter()
                            samples, spreads = asyncio.run(run_bursts(port, args.attempts, args.burst_size, args.timeout))
                            elapsed = time.perf_counter() - started
                        result["login"] = summarize(samples, elapsed)
                        result["login"].update(samples=samples, launch_spread_ms=spreads,
                                               resources=resources.result, resource_window_s=resources.elapsed)
                        result["echo"] = asyncio.run(measure_echo(port, args, pids, args.duration))
                        if proxy.poll() is not None or fixture.poll() is not None:
                            raise RuntimeError("proxy or fixture exited during measurement")
                        for workload in ("login", "echo"):
                            values = result[workload]
                            print(f"  {workload}: {values['succeeded']}/{values['attempts']} successful; "
                                  f"p95 {values['latency_p95_ms']} ms; "
                                  f"resources {values['resources']['proxy']}", flush=True)
                result["log_sha256"] = sha256(Path(result["log"]))
                result["fixture_log_sha256"] = sha256(log)
                report["aggregate"] = aggregate(report["results"])
                write_report(report, args.report)
        report["status"] = "complete"
        report["completed_at"] = datetime.now(timezone.utc).isoformat()
    except BaseException as error:
        report["status"] = "incomplete"
        report["error"] = f"{error_name(error)}: {error}"
        raise
    finally:
        report["aggregate"] = aggregate(report["results"])
        write_report(report, args.report)
    if args.require_success and report_has_failures(report):
        raise SystemExit("Comparison recorded request failures; see report")
    print(f"Report: {args.report}", flush=True)


if __name__ == "__main__":
    main()
