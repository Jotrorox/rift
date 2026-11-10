#!/usr/bin/env python3
"""End-to-end protocol translation tests against real servers.

For each cell (server fixture, client version) the harness starts the real
server (offline, superflat, creative, view distance 3, fresh temporary world),
puts the translating proxy in front of it, and connects two players at once: a
reference client directly to the server in the server's protocol, and a
translated client through the proxy in the client's protocol. A scripted
scenario of named, independent steps (tests/matrix_scenario.py) then produces
a broad mix of packets in both directions.

The heart of the check: every packet either client receives is strictly
decoded with the minecraft-data protocol.json of that client's own protocol
(tests/protodef.py; 26.2 and 26.3 schemas are derived in
tests/schema_patches.py). An unknown packet ID, a decode error or trailing
bytes fails the cell and is reported with state, packet name and ID, the step
that was running, the error and a hexdump. Steps also compare what both
clients saw after normalising registry IDs to names, and check serverbound
translation through the server's reactions.

Usage (Python 3.11+; use /opt/homebrew/bin/python3 on macOS):

    tests/translation_matrix.py --accept-eula --server vanilla-1.16.5 --client 1.17.1
    tests/translation_matrix.py --accept-eula --server paper-1.20.4 --client 1.12.2,1.16.5,1.21.4
    tests/translation_matrix.py --accept-eula --server vanilla-1.21.4          # client = server version
    tests/translation_matrix.py --accept-eula --matrix smoke                    # predefined sets
    tests/translation_matrix.py --accept-eula --server vanilla-1.20.1 --scenario blocks,items --trace
    tests/translation_matrix.py --list                                          # fixtures, steps, matrices

Options: --via proxy (default; crates/rift-translation's example, built with
cargo) or --via rift (the rift binary with a generated config declaring the
backend's version); --scenario all|join|name,name (join runs only login, the
mandatory teleport and idle steps); --keep keeps the server directory;
--trace passes --trace to the proxy (packet names on its stderr); --json FILE
writes machine-readable results; --direct also runs the reference scenario
with the "translated" client connected directly (validates the decoder).

A server is started once per --server and reused for every --client. At most
three Minecraft servers run at once machine-wide (RIFT_VIA_MAX_SERVERS); other
invocations wait for a slot. Downloads, Paper patching, Spigot BuildTools
builds and data-generator runs are cached under ~/.cache/rift-via (RIFT_VIA_CACHE)
and guarded by per-artifact locks. Logs of every cell (server log, proxy
stderr, both clients' decoded packets as JSON lines, result.json) are kept
under target/minecraft/runs/<timestamp>-<server>-<client>/. The exit status is
non-zero if any cell fails.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))

import matrix_env as env  # noqa: E402
import matrix_fixtures as fixtures  # noqa: E402
from matrix_client import Session, SessionError, hexdump  # noqa: E402
import matrix_registries  # noqa: E402
import matrix_scenario as scenario  # noqa: E402
import protodef  # noqa: E402

ROOT = fixtures.ROOT
RUNS = ROOT / "target" / "minecraft" / "runs"
PROXY = ROOT / "target" / "debug" / "examples" / "proxy"
ALWAYS = ("teleport", "op", "idle")  # Kept by --scenario join.


def release(protocol):
    return fixtures.RELEASE_OF[protocol]


def client_protocol(text):
    text = text.strip()
    if text.isdigit() and int(text) in fixtures.RELEASE_OF:
        return int(text)
    if text in fixtures.PROTOCOL_OF:
        return fixtures.PROTOCOL_OF[text]
    for key, fixture in fixtures.SERVERS.items():
        if key.startswith("vanilla") and fixture["version"] == text and fixture["protocol"] in fixtures.RELEASE_OF:
            return fixture["protocol"]
    raise SystemExit(f"unknown client version {text!r}; versions: "
                     + ", ".join(name for _, name in fixtures.RELEASES))


# --- Matrices ----------------------------------------------------------------

def vanilla(version):
    return f"vanilla-{version}"


def matrices():
    names = [name for _, name in fixtures.RELEASES]
    adjacent = {}
    for older, newer in zip(names, names[1:]):
        adjacent.setdefault(vanilla(older), []).append(newer)
        adjacent.setdefault(vanilla(newer), []).append(older)
    anchors = ["1.12.2", "1.16.5", "1.20.1", "1.21.4", "26.3"]
    software = {}
    for server in ["paper-1.12.2", "paper-1.16.5", "paper-1.20.4", "paper-1.21.11", "paper-26.3",
                   "spigot-1.12.2", "spigot-1.21.11", "spigot-26.3", "pumpkin"]:
        own = "26.3" if server == "pumpkin" else server.split("-", 1)[1]
        software[server] = list(dict.fromkeys([own, "1.12.2", "1.21.4", "26.3"]))
    return {
        "smoke": {vanilla("1.12.2"): ["1.12.2", "1.13"], vanilla("1.16.5"): ["1.16.5", "1.17"],
                  vanilla("1.21.4"): ["1.21.4", "1.21.5"], vanilla("26.3"): ["26.3"]},
        "direct": {vanilla(name): [name] for name in names},
        "adjacent": adjacent,
        "anchors": {vanilla(server): anchors for server in anchors},
        "software": software,
        "full": {vanilla(server): names for server in names},
    }


# --- Proxies -------------------------------------------------------------------

_build_lock = threading.Lock()
_built = {}


def build(target):
    """Build the proxy example (or rift) once per invocation."""
    with _build_lock:
        if target in _built:
            return _built[target]
        if target == "proxy":
            command = ["cargo", "build", "-p", "rift-translation", "--example", "proxy"]
            path = PROXY
        else:
            command = ["cargo", "build", "--release", "--locked"]
            path = ROOT / "target" / "release" / "rift"
        env.log(f"building {target}: {' '.join(command)}")
        result = subprocess.run(command, cwd=ROOT, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
        if result.returncode:
            raise SystemExit(f"{' '.join(command)} failed:\n{result.stdout[-4000:]}")
        _built[target] = path
        return path


class Proxy:
    """The translating proxy in front of one server."""

    def __init__(self, kind, server, directory, trace=False, binary=None):
        self.kind, self.server, self.directory, self.trace = kind, server, Path(directory), trace
        self.binary = binary
        self.log = self.directory / ("proxy.log" if kind == "proxy" else "rift.log")
        self.proc = self.port = None

    def start(self):
        self.port = env.allocate_port()
        if self.kind == "proxy":
            command = [str(self.binary or build("proxy")), "--listen", f"127.0.0.1:{self.port}",
                       "--backend", f"127.0.0.1:{self.server.port}",
                       "--backend-protocol", str(self.server.protocol)]
            if self.trace:
                command.append("--trace")
            ready = "translating"
        else:
            config = self.directory / "rift.lua"
            config.write_text(
                'local config = require("rift.config")\n'
                f'config.listeners.public = "127.0.0.1:{self.port}"\n'
                f'config.backends.main = {{ address = "127.0.0.1:{self.server.port}", '
                f'version = "{self.server.release}" }}\n'
                'config.routes.public = "main"\n')
            command = [str(self.binary or build("rift")), "--config", str(config)]
            ready = "rift: listening on"
        self.proc = env.PROCESSES.spawn(command, self.directory, self.log, stdin=False)
        deadline = time.monotonic() + 60
        while ready not in self.text():
            if self.proc.poll() is not None:
                raise RuntimeError(f"{self.kind} exited ({self.proc.returncode}):\n{self.text()[-3000:]}")
            if time.monotonic() > deadline:
                raise TimeoutError(f"{self.kind} did not start:\n{self.text()[-3000:]}")
            time.sleep(0.05)
        return self

    def text(self):
        try:
            return self.log.read_text(errors="replace")
        except FileNotFoundError:
            return ""

    def offset(self):
        return len(self.text())

    def stop(self):
        if self.proc is not None:
            env.PROCESSES.stop(self.proc, grace=3)
            self.proc = None
        if self.port is not None:
            env.release_port(self.port)
            self.port = None


# --- One cell ------------------------------------------------------------------

def player_name(prefix):
    return prefix + uuid.uuid4().hex[:7]


def selected_steps(selection):
    if selection in (None, "all"):
        return list(scenario.STEPS)
    if selection == "join":
        wanted = set(ALWAYS)
    else:
        wanted = {name.strip() for name in selection.split(",")}
        unknown = wanted - set(scenario.STEP_NAMES)
        if unknown:
            raise SystemExit(f"unknown scenario steps {sorted(unknown)}; steps: {', '.join(scenario.STEP_NAMES)}")
        wanted |= {"teleport"}  # Everything is built relative to the teleport target.
    return [entry for entry in scenario.STEPS if entry.name in wanted]


def decode_report(session, limit=8):
    """Decode failures grouped by (state, packet, error), with one example each."""
    groups = {}
    for record in session.failures():
        key = (record.state, record.name, record.id, record.error)
        groups.setdefault(key, []).append(record)
    report = []
    for (state, name, packet_id, error), records in groups.items():
        first = records[0]
        unverified = error.startswith("unverified") or "unverified" in error
        report.append(dict(
            level="UNVERIFIED" if unverified else "FAIL", state=state, packet=name, id=packet_id,
            error=error, count=len(records), step=first.step, size=len(first.body),
            hexdump=hexdump(first.body, 160)))
    report.sort(key=lambda entry: (entry["level"] != "FAIL", -entry["count"]))
    return report[:limit * 4]


# Every later cell on a reused server builds this much further along x, out
# of the view distance of the earlier build sites.
CELL_SPACING = 128


def reset_world(server):
    """Undo the global state an earlier cell's scenario left on a reused
    server (the scoreboard, team, boss bar and world border), so the next
    cell's commands change something and are seen. Runs before its players
    join; errors for things that do not exist are harmless."""
    cmd = scenario.Commands(server.protocol, server.software)
    lines = ["scoreboard objectives remove riftobj", cmd.team("remove", "riftteam"),
             "worldborder set 59999968", "worldborder warning distance 5"]
    if server.protocol >= 393:
        lines.append("bossbar remove rift:bar")
    for line in lines:
        server.console(line)


def run_cell(server, proxy, protocol, steps, label, directory, args, cell=0):
    """Run the scenario for one client protocol on a running server; `cell`
    counts the cells run on it before."""
    result = dict(server=server.name, server_version=server.release, server_protocol=server.protocol,
                  client=release(protocol), client_protocol=protocol, via=args.via,
                  status="FAIL", steps=[], findings=[], logs=str(directory))
    begun = time.monotonic()
    directory.mkdir(parents=True, exist_ok=True)
    log_start, proxy_start = server.offset(), proxy.offset() if proxy else 0
    ref = via = None
    try:
        if cell:
            reset_world(server)
        via_port = proxy.port if proxy else server.port
        try:
            ref_reg = matrix_registries.registries(server.protocol)
            via_reg = matrix_registries.registries(protocol)
        except Exception as error:  # Registries only matter for comparisons.
            env.log(f"registry names unavailable: {error}")
            ref_reg = via_reg = None
        pumpkin = server.software == "pumpkin"
        ref = Session(server.port, player_name("Ref"), server.protocol, label="reference", pumpkin=pumpkin)
        ref.join()
        via = Session(via_port, player_name("Via"), protocol, label="translated", pumpkin=pumpkin)
        try:
            via.join()
        except SessionError as error:
            raise SessionError(f"the translated client could not join: {error}") from None
        result["joined"] = round(time.monotonic() - begun, 1)
        x, y, z = (int(v // 1) for v in ref.position[:3])
        surface = fixtures.surface(server.protocol, server.software)
        base = (cell * CELL_SPACING, surface if surface is not None else y, 0)
        cx = scenario.Context(server, ref, via, ref_reg, via_reg, base)
        for entry in steps:
            reason = entry.applies(server.protocol, protocol, server.software)
            outcome = dict(name=entry.name, status="SKIP", seconds=0.0)
            if reason:
                outcome["reason"] = reason
                result["steps"].append(outcome)
                continue
            cx.step = entry.name
            ref.step = via.step = entry.name
            started = time.monotonic()
            before = len(cx.findings)
            try:
                cx.begin()
                entry.function(cx)
                failed = any(f["level"] == "FAIL" for f in cx.findings[before:])
                outcome["status"] = "FAIL" if failed else "PASS"
            except scenario.Skip as skip:
                outcome.update(status="SKIP", reason=str(skip))
            except (scenario.StepFailure, SessionError, AssertionError, KeyError, ValueError) as error:
                outcome["status"] = "FAIL"
                cx.fail(f"{type(error).__name__}: {error}" if not isinstance(
                    error, (scenario.StepFailure, SessionError)) else str(error))
                if isinstance(error, (KeyError, ValueError)):
                    cx.findings[-1]["traceback"] = traceback.format_exc()[-1500:]
            outcome["seconds"] = round(time.monotonic() - started, 2)
            result["steps"].append(outcome)
            # A dead connection makes later steps meaningless.
            if ref.failure or via.failure:
                break
        ref.step = via.step = "end"
        result["findings"] = cx.findings
        for session in (ref, via):
            if session.failure:
                result["findings"].append(dict(level="FAIL", step=session.step,
                                               message=f"{session.label}: {session.failure}"))
    except Exception as error:
        result["findings"].append(dict(level="FAIL", step="setup", message=f"{type(error).__name__}: {error}",
                                       traceback=traceback.format_exc()[-2000:]))
    finally:
        for session in (via, ref):
            if session is not None:
                session.close()
    time.sleep(0.5)
    server_log = server.text()[log_start:]
    (directory / "server.log").write_text(server_log)
    if proxy:
        proxy_log = proxy.text()[proxy_start:]
        (directory / proxy.log.name).write_text(proxy_log)
        errors = [line for line in proxy_log.splitlines()
                  if "failed" in line or "cannot" in line.lower() or "error" in line.lower()]
        if errors:
            result["proxy_errors"] = errors[-20:]
    for session, name in ((ref, "reference"), (via, "translated")):
        if session is None:
            continue
        session.write_log(directory / f"{name}.jsonl")
        report = decode_report(session)
        result[name] = dict(protocol=session.protocol, packets=len(session.records),
                            sent=len(session.sent), keepalives=session.keepalives,
                            decode_failures=sum(entry["count"] for entry in report if entry["level"] == "FAIL"),
                            unverified=sum(entry["count"] for entry in report if entry["level"] == "UNVERIFIED"),
                            failures=report)
        for entry in report:
            result["findings"].append(dict(
                level=entry["level"], step=entry["step"],
                message=(f"{name} client could not decode {entry['state']} {entry['packet'] or '?'} "
                         f"(id {entry['id']:#04x}, {entry['size']} bytes, x{entry['count']}): {entry['error']}"),
                hexdump=entry["hexdump"]))
    kicked = [line for line in server_log.splitlines()
              if ("lost connection" in line or "Disconnecting" in line or "kicked" in line.lower())
              and (ref and ref.name in line or via and via.name in line)
              and "Disconnected" not in line]
    if kicked:
        result["findings"].append(dict(level="FAIL", step="server", message="server log: " + " | ".join(kicked[-3:])))
    failed = [f for f in result["findings"] if f["level"] == "FAIL"]
    result["status"] = "FAIL" if failed or any(s["status"] == "FAIL" for s in result["steps"]) else "PASS"
    result["seconds"] = round(time.monotonic() - begun, 1)
    (directory / "result.json").write_text(json.dumps(result, indent=2, default=str) + "\n")
    return result


def run_server(name, clients, args, steps):
    """Start one server (and proxy) and run every client against it."""
    stamp = time.strftime("%Y%m%d-%H%M%S")
    results = []
    workdir = Path(tempfile.mkdtemp(prefix=f"rift-matrix-{name}-"))
    server = fixtures.Server(name, workdir / "server")
    proxy = None
    try:
        started = time.monotonic()
        server.start()
        env.log(f"{server.name} ({server.release}, protocol {server.protocol}) ready in {server.started}s "
                f"on port {server.port}")
        if args.via != "direct":
            proxy = Proxy(args.via, server, workdir, args.trace, args.binary).start()
        for cell, protocol in enumerate(clients or [server.protocol]):
            label = f"{server.name}-client-{release(protocol)}"
            directory = RUNS / f"{stamp}-{label}"
            result = run_cell(server, proxy, protocol, steps, label, directory, args, cell)
            result["server_start"] = server.started
            results.append(result)
            print(summary_line(result), flush=True)
            if result["status"] != "PASS":
                print(failure_text(result), flush=True)
        del started
    except Exception as error:
        detail = traceback.format_exc()
        for protocol in clients or [server.protocol]:
            if not any(r["client_protocol"] == protocol for r in results):
                result = dict(server=name, client=release(protocol), client_protocol=protocol, via=args.via,
                              status="ERROR", steps=[], seconds=0,
                              findings=[dict(level="FAIL", step="server", message=f"{error}", traceback=detail[-3000:])])
                results.append(result)
                print(summary_line(result) + f"\n    {error}", flush=True)
    finally:
        if proxy:
            proxy.stop()
        server.stop()
        if args.keep:
            env.log(f"kept server directory {workdir}")
        else:
            shutil.rmtree(workdir, ignore_errors=True)
    return results


# --- Output --------------------------------------------------------------------

def summary_line(result):
    counts = {}
    for entry in result.get("steps", []):
        counts[entry["status"]] = counts.get(entry["status"], 0) + 1
    via = result.get("translated", {})
    infos = sum(1 for f in result.get("findings", []) if f["level"] == "INFO")
    unverified = via.get("unverified", 0)
    return (f"{result['status']:5} {result['server']:>22} <- client {result['client']:<8} "
            f"steps {counts.get('PASS', 0)} pass/{counts.get('FAIL', 0)} fail/{counts.get('SKIP', 0)} skip  "
            f"packets {via.get('packets', 0)} (decode failures {via.get('decode_failures', 0)}"
            f"{f', unverified {unverified}' if unverified else ''}; reference "
            f"{result.get('reference', {}).get('decode_failures', 0)})  info {infos}  "
            f"{result.get('seconds', 0)}s  {result.get('logs', '')}")


def failure_text(result, limit=20):
    lines = []
    for finding in [f for f in result.get("findings", []) if f["level"] in ("FAIL", "UNVERIFIED")][:limit]:
        lines.append(f"    [{finding['level']}] {finding.get('step')}: {finding['message']}")
        if finding.get("hexdump"):
            lines.extend("        " + line for line in finding["hexdump"].splitlines()[:6])
        if finding.get("traceback"):
            lines.extend("        " + line for line in finding["traceback"].splitlines()[-4:])
    if result.get("proxy_errors"):
        lines.append("    proxy stderr:")
        lines.extend("        " + line for line in result["proxy_errors"][-8:])
    return "\n".join(lines)


def table(results):
    lines = [f"{'server':>22} {'client':<8} {'result':<6} {'steps p/f/s':<12} {'packets':>7} "
             f"{'decode':>6} {'unver':>5} {'secs':>6}"]
    for result in results:
        steps = result.get("steps", [])
        count = lambda status: sum(1 for entry in steps if entry["status"] == status)
        via = result.get("translated", {})
        lines.append(f"{result['server']:>22} {result['client']:<8} {result['status']:<6} "
                     f"{count('PASS')}/{count('FAIL')}/{count('SKIP'):<8} {via.get('packets', 0):>7} "
                     f"{via.get('decode_failures', 0):>6} {via.get('unverified', 0):>5} {result.get('seconds', 0):>6}")
    passed = sum(1 for result in results if result["status"] == "PASS")
    lines.append(f"{passed}/{len(results)} cells passed")
    return "\n".join(lines)


def redecode(directory):
    """Decode a finished run's failed packets again with the current schema
    and corrections (no servers needed)."""
    directory = Path(directory)
    result = json.loads((directory / "result.json").read_text())
    remaining = 0
    for name, key in (("reference", "server_protocol"), ("translated", "client_protocol")):
        path = directory / f"{name}.jsonl"
        if not path.exists():
            continue
        protocol = result[key]
        schema = protodef.schema_for(protocol)
        decoders = {}
        for line in path.read_text().splitlines():
            entry = json.loads(line)
            if "error" not in entry:
                continue
            state = entry["state"]
            decoder = decoders.setdefault(state, protodef.Decoder(schema, state, "clientbound"))
            body = bytes.fromhex(entry["hex"])
            try:
                packet, value = decoder.packet(entry["id"], body)
                print(f"{name} #{entry['i']} {packet}: now decodes")
            except protodef.DecodeError as error:
                remaining += 1
                print(f"{name} #{entry['i']} {state} {entry['name']} ({len(body)} bytes, step {entry['step']}): "
                      f"{error}\n{hexdump(body, 256)}")
    raise SystemExit(1 if remaining else 0)


def listing():
    print("Server fixtures (tests/servers.json):")
    for key, fixture in fixtures.SERVERS.items():
        if fixture["protocol"] >= 335:
            how = "BuildTools" if fixture.get("build") else "download"
            print(f"  {key:<24} {fixture['version']:<20} protocol {fixture['protocol']:<4} java {fixture['java']:<3} {how}")
    print("Client versions:", ", ".join(name for _, name in fixtures.RELEASES))
    print("Scenario steps:")
    for entry in scenario.STEPS:
        limits = []
        if entry.min_server:
            limits.append(f"server>={entry.min_server}")
        if entry.min_client:
            limits.append(f"client>={entry.min_client}")
        if entry.software != scenario.ALL:
            limits.append("/".join(entry.software))
        print(f"  {entry.name:<28} {entry.doc} {('(' + ', '.join(limits) + ')') if limits else ''}")
    print("Matrices:")
    for name, cells in matrices().items():
        print(f"  {name:<9} {sum(len(clients) for clients in cells.values())} cells on {len(cells)} servers")


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0],
                                     formatter_class=argparse.RawDescriptionHelpFormatter, epilog=__doc__)
    parser.add_argument("--accept-eula", action="store_true",
                        help="accept https://aka.ms/MinecraftEULA for the local test servers")
    parser.add_argument("--server", action="append", default=[],
                        help="server fixture, e.g. vanilla-1.16.5, paper-1.20.4, spigot-1.12.2, pumpkin "
                             "(repeat or comma-separate)")
    parser.add_argument("--client", default=None, help="client versions, comma-separated (default: the server's)")
    parser.add_argument("--matrix", choices=sorted(matrices()), help="a predefined set of cells")
    parser.add_argument("--via", choices=["proxy", "rift", "direct"], default="proxy",
                        help="proxy: the rift-translation example; rift: the rift binary; "
                             "direct: no proxy (validates the decoder and scenario)")
    parser.add_argument("--binary", type=Path, help="use this proxy/rift binary instead of building")
    parser.add_argument("--scenario", default="all", help="all, join, or comma-separated step names")
    parser.add_argument("--jobs", type=int, default=1, help="servers to run in parallel from this process")
    parser.add_argument("--keep", action="store_true", help="keep server directories")
    parser.add_argument("--trace", action="store_true", help="pass --trace to the proxy")
    parser.add_argument("--json", type=Path, help="write results as JSON")
    parser.add_argument("--list", action="store_true", help="list fixtures, steps and matrices")
    parser.add_argument("--redecode", type=Path, metavar="RUN_DIR",
                        help="decode a finished run's failed packets again with the current schemas")
    args = parser.parse_args()
    if args.list:
        listing()
        return
    if args.redecode:
        redecode(args.redecode)
    if not args.accept_eula:
        parser.error("--accept-eula is required to run the Minecraft servers")
    env.install_signal_handlers()
    steps = selected_steps(args.scenario)
    plan = {}
    if args.matrix:
        for server, clients in matrices()[args.matrix].items():
            plan[server] = [client_protocol(client) for client in clients]
    servers = [name for entry in args.server for name in entry.split(",") if name]
    clients = [client_protocol(c) for c in args.client.split(",")] if args.client else None
    for server in servers:
        plan[server] = clients
    if not plan:
        parser.error("give --server or --matrix")
    for server in plan:
        fixtures.resolve(server)
    if args.via == "direct":
        for server, wanted in plan.items():
            protocol = fixtures.SERVERS[fixtures.resolve(server)]["protocol"]
            if wanted and set(wanted) != {protocol}:
                parser.error("--via direct connects clients of the server's own version only")
    if args.binary:
        args.binary = args.binary.resolve(strict=True)
    elif args.via in ("proxy", "rift"):
        build(args.via)
    RUNS.mkdir(parents=True, exist_ok=True)
    began = time.monotonic()
    results = []
    try:
        with ThreadPoolExecutor(max_workers=max(1, args.jobs)) as pool:
            for batch in pool.map(lambda item: run_server(item[0], item[1], args, steps), plan.items()):
                results.extend(batch)
    except KeyboardInterrupt:
        env.log("interrupted; stopping servers")
        env.PROCESSES.stop_all()
        raise SystemExit(130)
    if len(results) > 1:
        print(table(results))
    print(f"total {round(time.monotonic() - began, 1)}s")
    if args.json:
        args.json.parent.mkdir(parents=True, exist_ok=True)
        args.json.write_text(json.dumps(results, indent=2, default=str) + "\n")
    if any(result["status"] != "PASS" for result in results):
        raise SystemExit(1)


if __name__ == "__main__":
    os.environ.setdefault("PYTHONUNBUFFERED", "1")
    main()
