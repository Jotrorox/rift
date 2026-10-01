#!/usr/bin/env python3
"""Automatic service capacity and bounded recovery with real managed processes."""

import argparse
from contextlib import ExitStack, closing
import json
import os
from pathlib import Path
import signal
import socket
import sys
import tempfile
import time

from managed_wire import SECRET, admin, eventually
from minecraft import ROOT, process, unused_port, wait_ready
from network_wire import Backend, NetworkClient
from services_wire import available_range


def child(port, name):
    with Path("starts.txt").open("a") as output:
        output.write(f"{os.getpid()}\n")
    if Path("fail-start").exists():
        raise SystemExit(17)
    with closing(Backend(name, port, port=port)):
        for line in sys.stdin:
            if line.strip() == "stop":
                Path("stopped.txt").write_text(str(os.getpid()))
                break


def command():
    arguments = [sys.executable, str(Path(__file__).resolve()),
                 "--child", "{port}", "--name", "{name}"]
    return ",".join(json.dumps(argument) for argument in arguments)


def operation(control, *arguments):
    response = admin(control, *arguments)
    assert response["ok"], response
    return response["data"]


def servers(control):
    return {server["name"]: server
            for server in operation(control, "servers")["servers"]}


def allocate_ports(excluded=()):
    ports = set(excluded)
    result = []
    while len(result) < 2:
        candidate = unused_port()
        if candidate not in ports:
            ports.add(candidate)
            result.append(candidate)
    return result


def show_logs(directory):
    print((directory / "proxy.log").read_text(), flush=True)
    for path in directory.glob("servers/*/rift-server.log"):
        print(f"{path}:\n{path.read_text()}", flush=True)


def scaling(binary):
    with tempfile.TemporaryDirectory(prefix="rift-scaling-wire-") as temporary:
        directory = Path(temporary)
        first = available_range()
        front, control = allocate_ports(range(first, first + 5))
        config = directory / "rift.lua"
        config.write_text(f"""return {{
          listeners={{public='127.0.0.1:{front}'}}, backends={{}}, routes={{public='lobby'}},
          network={{hubs={{'lobby'}}}}, shutdown_timeout_ms=1000,
          admin={{listen='127.0.0.1:{control}',permissions={{'servers','shutdown','reload'}}}},
          service_groups={{lobby={{command={{{command()}}},directory='servers/{{name}}',
            port_range={{{first},{first+4}}}, start_timeout_ms=5000,stop_timeout_ms=1000,
            restart_delay_ms=100,restart_retries=2,
            scaling={{min_instances=1,max_instances=3,spare_instances=1,
              capacity_per_instance=2,target_occupancy_percent=80,
              queue_threshold=1,cooldown_ms=1500}}}}}}
        }}""")
        environment = dict(os.environ, RIFT_ADMIN_TOKEN=SECRET)
        log = directory / "proxy.log"
        with process([str(binary), "--config", str(config)], directory, log.name,
                     env=environment) as proxy:
            try:
                wait_ready(proxy, lambda: "rift: admin on" in log.read_text(), log, timeout=10)

                def running(count):
                    current = servers(control)
                    return current if len(current) == count and all(
                        server["state"] == "running" for server in current.values()) else None

                minimum = eventually(lambda: running(1), "minimum/spare instance starts without logins")
                assert next(iter(minimum)) == "lobby-1", minimum
                group = operation(control, "groups")["groups"][0]
                assert group["scaling"] == {
                    "min_instances": 1, "max_instances": 3, "spare_instances": 1,
                    "capacity_per_instance": 2, "target_occupancy_percent": 80,
                    "queue_threshold": 1, "cooldown_ms": 1500,
                }, group
                with ExitStack() as clients:
                    alice = clients.enter_context(NetworkClient(front, "ScaleAlice"))
                    alice.joined("lobby-1")
                    alice.probe("lobby-1")
                    eventually(lambda: running(2), "one occupied instance receives a spare")
                    bob = clients.enter_context(NetworkClient(front, "ScaleBob"))
                    bob.joined("lobby-2")
                    bob.probe("lobby-2")
                    carol = clients.enter_context(NetworkClient(front, "ScaleCarol"))
                    carol.joined("lobby-1")
                    carol.probe("lobby-1")
                    full = eventually(lambda: running(3), "occupancy grows capacity to maximum")
                    assert set(full) == {"lobby-1", "lobby-2", "lobby-3"}, full
                    assert not admin(control, "create", "lobby")["ok"], "manual create must honor maximum"
                    # Admissions beyond the maximum must not create more instances.
                    dave = clients.enter_context(NetworkClient(front, "ScaleDave"))
                    dave.joined("lobby-3")
                    dave.probe("lobby-3")
                    time.sleep(0.4)
                    assert len(servers(control)) == 3, servers(control)
                    pids = {name: value["pid"] for name, value in servers(control).items()}
                    operation(control, "reload")
                    assert {name: value["pid"] for name, value in servers(control).items()} == pids
                    alice.probe("lobby-1")
                    bob.probe("lobby-2")
                    carol.probe("lobby-1")
                    dave.probe("lobby-3")
                    # Removing some load leaves occupied instances protected.
                    dave.__exit__()
                    carol.__exit__()
                    eventually(lambda: sum(value["players"] for value in servers(control).values()) == 2,
                               "partial disconnect releases player occupancy")
                    retained = eventually(lambda: running(2), "shrink removes only the empty instance")
                    assert set(retained) == {"lobby-1", "lobby-2"}, retained
                    alice.probe("lobby-1")
                    bob.probe("lobby-2")
                eventually(lambda: all(value["players"] == 0 and value["reservations"] == 0
                                       for value in servers(control).values()),
                           "all player attachments release")
                time.sleep(0.2)
                assert len(servers(control)) == 2, "cooldown must defer another scale-down"
                reduced = eventually(lambda: running(1), "empty instances shrink to minimum after cooldown")
                assert len(list(directory.glob("servers/*/stopped.txt"))) >= 2
                assert all((directory / "servers" / name / "starts.txt").exists()
                           for name in full), "automatic removal must preserve persistent files"
                time.sleep(0.2)
                assert len(servers(control)) == 1, "minimum must be retained while idle"
                final_name, final = next(iter(reduced.items()))
                operation(control, "shutdown")
                proxy.wait(timeout=8)
                assert proxy.returncode == 0
                assert (directory / "servers" / final_name / "stopped.txt").read_text() == str(final["pid"])
                for port in range(first, first + 5):
                    with socket.socket() as probe:
                        assert probe.connect_ex(("127.0.0.1", port)) != 0
            except BaseException:
                show_logs(directory)
                raise


def recovery(binary):
    with tempfile.TemporaryDirectory(prefix="rift-recovery-wire-") as temporary:
        directory = Path(temporary)
        server_directory = directory / "servers/recovery"
        server_directory.mkdir(parents=True)
        front, control = allocate_ports()
        backend = unused_port()
        while backend in (front, control):
            backend = unused_port()
        arguments = [sys.executable, str(Path(__file__).resolve()),
                     "--child", str(backend), "--name", "recovery"]
        argv = ",".join(json.dumps(argument) for argument in arguments)
        config = directory / "rift.lua"
        config.write_text(f"""return {{
          listeners={{public='127.0.0.1:{front}'}},
          backends={{recovery='127.0.0.1:{backend}'}},routes={{public='recovery'}},
          shutdown_timeout_ms=1000,
          admin={{listen='127.0.0.1:{control}',permissions={{'servers','shutdown'}}}},
          managed_servers={{recovery={{command={{{argv}}},directory='servers/recovery',
            autostart=true,start_timeout_ms=3000,stop_timeout_ms=1000,
            restart_delay_ms=150,restart_retries=2}}}}
        }}""")
        log = directory / "proxy.log"
        environment = dict(os.environ, RIFT_ADMIN_TOKEN=SECRET)
        with process([str(binary), "--config", str(config)], directory, log.name,
                     env=environment) as proxy:
            try:
                wait_ready(proxy, lambda: "rift: admin on" in log.read_text(), log, timeout=10)

                def state():
                    return servers(control)["recovery"]

                def starts():
                    path = server_directory / "starts.txt"
                    return path.read_text().splitlines() if path.exists() else []

                initial = eventually(lambda: state() if state()["state"] == "running" else None,
                                     "autostart running")
                # A real process crash recovers with no new player demand.
                os.kill(initial["pid"], signal.SIGTERM)
                recovered = eventually(lambda: state() if state()["state"] == "running"
                                       and state()["pid"] != initial["pid"] else None,
                                       "unexpected exit restarts automatically")
                assert len(starts()) == 2
                # Fail all subsequent starts to exhaust a finite retry budget.
                (server_directory / "fail-start").touch()
                os.kill(recovered["pid"], signal.SIGTERM)
                eventually(lambda: state()["state"] == "failed" and len(starts()) >= 3,
                           "repeated startup failures become visible")
                exhausted_state = eventually(lambda: state() if state()["restart_exhausted"] else None,
                                             "bounded recovery reports exhaustion")
                assert exhausted_state["restart_attempts"] == 2, exhausted_state
                assert exhausted_state["last_error"], exhausted_state
                exhausted = len(starts())
                assert exhausted == 3, "initial start plus exactly two permitted restarts"
                time.sleep(0.7)
                assert len(starts()) == exhausted, "retry exhaustion must stop process creation"
                with NetworkClient(front, "Exhausted") as client:
                    client.until("disconnect")
                time.sleep(0.2)
                assert len(starts()) == exhausted, "new demand must honor retry exhaustion"
                (server_directory / "fail-start").unlink()
                operation(control, "start", "recovery")
                restored = eventually(lambda: state() if state()["state"] == "running" else None,
                                      "manual start resets exhausted retry budget")
                assert len(starts()) == exhausted + 1
                assert restored["restart_attempts"] == 0 and not restored["restart_exhausted"], restored
                operation(control, "stop", "recovery")
                eventually(lambda: state()["state"] == "stopped", "explicit stop completes")
                assert (server_directory / "stopped.txt").read_text() == str(restored["pid"])
                time.sleep(0.7)
                assert state()["state"] == "stopped" and not state()["automatic_enabled"]
                assert len(starts()) == exhausted + 1, "manual stop must disable automatic recovery"
                operation(control, "shutdown")
                proxy.wait(timeout=8)
                assert proxy.returncode == 0
                with socket.socket() as probe:
                    assert probe.connect_ex(("127.0.0.1", backend)) != 0
            except BaseException:
                show_logs(directory)
                raise


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/rift")
    parser.add_argument("--child", type=int)
    parser.add_argument("--name", default="lobby")
    args = parser.parse_args()
    if not __debug__:
        parser.error("assertions must be enabled")
    if args.child:
        child(args.child, args.name)
    else:
        scaling(args.binary.resolve())
        recovery(args.binary.resolve())
        print("PASS: automatic minimum/spare capacity, occupancy growth, maximum, empty shrink, reload, bounded recovery, manual reset/stop, cleanup")
